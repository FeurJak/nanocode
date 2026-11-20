use std::collections::BTreeMap;

use serde_json::{Value, json};

const FRESH_MS: u64 = 15 * 60 * 1000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pin {
    Taskset,
    Nice,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Placement {
    Fill,
    LastResort,
    Named,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pool {
    Large,
    Small,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Machine {
    pub name: String,
    pub cores: u32,
    pub expires_ms: Option<u64>,
    pub pin: Pin,
    pub placement: Placement,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Observation {
    pub load: f64,
    pub cores: u32,
    pub mem_total_kb: u64,
    pub mem_available_kb: u64,
    pub at_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Lease {
    pub id: String,
    pub machine: String,
    pub first_cpu: u32,
    pub cores: u32,
    pub label: String,
    pub taken_ms: u64,
    pub until_ms: u64,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct State {
    pub observations: BTreeMap<String, Observation>,
    pub leases: Vec<Lease>,
    pub next: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Usage {
    pub cores: u32,
    pub leased: u32,
    pub busy: u32,
    pub memory_ok: bool,
    pub observed: bool,
}

impl Lease {
    pub fn cpus(&self) -> String {
        format!("{}-{}", self.first_cpu, self.first_cpu + self.cores - 1)
    }

    pub fn to_json(&self, machine: Option<&Machine>) -> Value {
        let prefix = match machine.map(|machine| machine.pin) {
            Some(Pin::Nice) => "nice -n 19".to_owned(),
            _ => format!("taskset -c {}", self.cpus()),
        };
        json!({
            "lease": self.id,
            "machine": self.machine,
            "hand": self.machine,
            "cores": self.cores,
            "cpus": self.cpus(),
            "prefix": prefix,
            "label": self.label,
            "taken_ms": self.taken_ms,
            "until_ms": self.until_ms,
        })
    }
}

fn number(value: &Value, name: &str) -> Option<u64> {
    value.get(name)?.as_u64()
}

fn text(value: &Value, name: &str) -> Option<String> {
    value.get(name)?.as_str().map(str::to_owned)
}

impl State {
    pub fn from_json(json: &str) -> Option<Self> {
        if json.trim().is_empty() {
            return Some(Self::default());
        }
        let value: Value = serde_json::from_str(json).ok()?;
        let mut observations = BTreeMap::new();
        for (name, observation) in value.get("observations")?.as_object()? {
            observations.insert(
                name.clone(),
                Observation {
                    load: observation.get("load")?.as_f64()?,
                    cores: u32::try_from(number(observation, "cores")?).ok()?,
                    mem_total_kb: number(observation, "mem_total_kb")?,
                    mem_available_kb: number(observation, "mem_available_kb")?,
                    at_ms: number(observation, "at_ms")?,
                },
            );
        }
        let mut leases = Vec::new();
        for lease in value.get("leases")?.as_array()? {
            leases.push(Lease {
                id: text(lease, "id")?,
                machine: text(lease, "machine")?,
                first_cpu: u32::try_from(number(lease, "first_cpu")?).ok()?,
                cores: u32::try_from(number(lease, "cores")?).ok()?,
                label: text(lease, "label")?,
                taken_ms: number(lease, "taken_ms")?,
                until_ms: number(lease, "until_ms")?,
            });
        }
        Some(Self {
            observations,
            leases,
            next: number(&value, "next")?,
        })
    }

    pub fn to_json(&self) -> String {
        let observations: serde_json::Map<String, Value> = self
            .observations
            .iter()
            .map(|(name, observation)| {
                (
                    name.clone(),
                    json!({
                        "load": observation.load,
                        "cores": observation.cores,
                        "mem_total_kb": observation.mem_total_kb,
                        "mem_available_kb": observation.mem_available_kb,
                        "at_ms": observation.at_ms,
                    }),
                )
            })
            .collect();
        let leases: Vec<Value> = self
            .leases
            .iter()
            .map(|lease| {
                json!({
                    "id": lease.id,
                    "machine": lease.machine,
                    "first_cpu": lease.first_cpu,
                    "cores": lease.cores,
                    "label": lease.label,
                    "taken_ms": lease.taken_ms,
                    "until_ms": lease.until_ms,
                })
            })
            .collect();
        serde_json::to_string_pretty(&json!({
            "observations": observations,
            "leases": leases,
            "next": self.next,
        }))
        .unwrap_or_default()
    }

    pub fn expire(&mut self, now_ms: u64) -> Vec<Lease> {
        let (expired, live) = self
            .leases
            .drain(..)
            .partition(|lease| lease.until_ms <= now_ms);
        self.leases = live;
        expired
    }

    pub fn usage(&self, machine: &Machine, now_ms: u64) -> Usage {
        let leased = self
            .leases
            .iter()
            .filter(|lease| lease.machine == machine.name)
            .map(|lease| lease.cores)
            .sum();
        let fresh = self
            .observations
            .get(&machine.name)
            .filter(|observation| now_ms.saturating_sub(observation.at_ms) <= FRESH_MS);
        let load = fresh.map_or(0, |observation| observation.load.ceil() as u32);
        let memory_ok = fresh.is_none_or(|observation| {
            observation.mem_available_kb.saturating_mul(10) >= observation.mem_total_kb
        });
        Usage {
            cores: fresh.map_or(machine.cores, |observation| {
                observation.cores.min(machine.cores)
            }),
            leased,
            busy: load.max(leased),
            memory_ok,
            observed: fresh.is_some(),
        }
    }

    fn gap(&self, machine: &Machine, cores: u32, want: u32) -> Option<u32> {
        let mut used: Vec<(u32, u32)> = self
            .leases
            .iter()
            .filter(|lease| lease.machine == machine.name)
            .map(|lease| (lease.first_cpu, lease.first_cpu + lease.cores))
            .collect();
        used.sort_unstable();
        let mut start = 0;
        for (first, end) in used {
            if first >= start + want {
                break;
            }
            start = start.max(end);
        }
        (start + want <= cores).then_some(start)
    }

    pub fn take(
        &mut self,
        machines: &[Machine],
        want: u32,
        label: &str,
        only: Option<&str>,
        now_ms: u64,
        ttl_ms: u64,
    ) -> Option<Lease> {
        self.expire(now_ms);
        for machine in machines {
            if only.is_some_and(|name| name != machine.name)
                || machine
                    .expires_ms
                    .is_some_and(|expires| expires <= now_ms + ttl_ms.min(10 * 60 * 1000))
            {
                continue;
            }
            let usage = self.usage(machine, now_ms);
            if !usage.memory_ok
                || usage.busy + want > usage.cores
                || (machine.placement == Placement::LastResort && !usage.observed)
                || (machine.placement == Placement::Named && only.is_none())
            {
                continue;
            }
            let Some(first_cpu) = self.gap(machine, usage.cores, want) else {
                continue;
            };
            self.next += 1;
            let lease = Lease {
                id: format!("{now_ms}-{}", self.next),
                machine: machine.name.clone(),
                first_cpu,
                cores: want,
                label: label.to_owned(),
                taken_ms: now_ms,
                until_ms: now_ms + ttl_ms,
            };
            self.leases.push(lease.clone());
            return Some(lease);
        }
        None
    }

    pub fn give(&mut self, id: &str) -> Option<Lease> {
        let at = self.leases.iter().position(|lease| lease.id == id)?;
        Some(self.leases.remove(at))
    }

    pub fn renew(&mut self, id: &str, until_ms: u64) -> Option<&Lease> {
        let lease = self.leases.iter_mut().find(|lease| lease.id == id)?;
        lease.until_ms = until_ms;
        Some(lease)
    }
}

const fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let day_of_year = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

pub fn parse_local_minutes(text: &str, utc_offset_minutes: i64) -> Option<u64> {
    let (date, time) = text.trim().split_once(' ')?;
    let mut date = date.split('-').map(str::parse::<i64>);
    let (year, month, day) = (date.next()?.ok()?, date.next()?.ok()?, date.next()?.ok()?);
    let (hour, minute) = time.split_once(':')?;
    let minutes = days_from_civil(year, month, day) * 1440
        + hour.parse::<i64>().ok()? * 60
        + minute.parse::<i64>().ok()?
        - utc_offset_minutes;
    u64::try_from(minutes * 60_000).ok()
}

pub fn machines_from_boxes(
    json: &str,
    pool: Pool,
    extra: &[Machine],
    last: &[Machine],
) -> Vec<Machine> {
    let boxes: Vec<Machine> = serde_json::from_str::<Value>(json)
        .ok()
        .and_then(|value| value.as_array().cloned())
        .unwrap_or_default()
        .iter()
        .filter_map(|entry| {
            Some(Machine {
                name: text(entry, "hand_name").or_else(|| text(entry, "name"))?,
                cores: u32::try_from(number(entry, "cores")?).ok()?,
                expires_ms: text(entry, "expires_at_pst")
                    .and_then(|expires| parse_local_minutes(&expires, -8 * 60)),
                pin: Pin::Taskset,
                placement: Placement::Fill,
            })
        })
        .collect();
    let large = boxes.iter().map(|machine| machine.cores).max().unwrap_or(0);
    let (larges, mut smalls): (Vec<Machine>, Vec<Machine>) = boxes
        .into_iter()
        .partition(|machine| machine.cores == large);
    smalls.sort_by_key(|machine| std::cmp::Reverse(machine.cores));
    let mut ordered = Vec::new();
    match pool {
        Pool::Large => {
            ordered.extend(larges);
            ordered.extend(smalls);
            ordered.extend(last.iter().cloned());
        }
        Pool::Small => {
            ordered.extend(smalls);
            ordered.extend(last.iter().cloned());
            ordered.extend(larges);
        }
    }
    ordered.extend(extra.iter().cloned());
    ordered
}
