use std::collections::BTreeMap;

pub type CpuTimes = BTreeMap<u32, (u64, u64)>;

pub struct LeaseCpus {
    pub id: String,
    pub first: u32,
    pub last: u32,
    pub kind: String,
}

pub struct IdleLease {
    pub id: String,
    pub busy_percent: u64,
    pub samples: u32,
    pub give: bool,
}

pub fn cpu_times(stat: &str) -> CpuTimes {
    stat.lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let cpu = fields.next()?.strip_prefix("cpu")?.parse().ok()?;
            let values = fields
                .take(8)
                .map(str::parse::<u64>)
                .collect::<Result<Vec<_>, _>>()
                .ok()?;
            let idle = values.get(3)? + values.get(4)?;
            Some((cpu, (values.iter().sum(), idle)))
        })
        .collect()
}

pub fn busy_percent(before: &CpuTimes, after: &CpuTimes, first: u32, last: u32) -> Option<u64> {
    let (mut total, mut idle) = (0u64, 0u64);
    for cpu in first..=last {
        let (total_before, idle_before) = before.get(&cpu)?;
        let (total_after, idle_after) = after.get(&cpu)?;
        total += total_after.checked_sub(*total_before)?;
        idle += idle_after.checked_sub(*idle_before)?;
    }
    (total > 0).then(|| total.saturating_sub(idle) * 100 / total)
}

pub fn leases_on(listing: &str, machine: &str) -> Vec<LeaseCpus> {
    listing
        .lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let ["lease", id, on, "cpus", cpus, kind, ..] = fields.as_slice() else {
                return None;
            };
            if *on != machine {
                return None;
            }
            let (first, last) = cpus.split_once('-').unwrap_or((cpus, cpus));
            Some(LeaseCpus {
                id: (*id).to_owned(),
                first: first.parse().ok()?,
                last: last.parse().ok()?,
                kind: (*kind).to_owned(),
            })
        })
        .collect()
}

pub fn idle_step(
    runs: &mut BTreeMap<String, u32>,
    leases: &[LeaseCpus],
    before: &CpuTimes,
    after: &CpuTimes,
    below_percent: u64,
    samples: u32,
    kept_kind: &str,
) -> Vec<IdleLease> {
    runs.retain(|id, _| leases.iter().any(|lease| &lease.id == id));
    let mut idle = Vec::new();
    for lease in leases {
        let Some(busy) = busy_percent(before, after, lease.first, lease.last) else {
            continue;
        };
        if busy >= below_percent {
            runs.remove(&lease.id);
            continue;
        }
        let run = runs.entry(lease.id.clone()).or_insert(0);
        *run += 1;
        let give = lease.kind != kept_kind;
        if *run == samples || (give && *run > samples) {
            idle.push(IdleLease {
                id: lease.id.clone(),
                busy_percent: busy,
                samples: *run,
                give,
            });
        }
    }
    idle
}
