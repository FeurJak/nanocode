use std::fmt::Write as _;

use super::*;

const FILE: &str = "defaults.json";
const KEYS: &str = "harness, model, thinking, tier";
const TIER_NAMES: &str = "standard, fast, ultrafast";

#[derive(Clone, Debug, Default, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Defaults {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier: Option<String>,
}

impl Defaults {
    pub fn harness(&self) -> Result<Option<HarnessArg>> {
        self.harness
            .as_deref()
            .map(|value| {
                HarnessArg::named(value)
                    .ok_or_else(|| format!("saved default harness {value:?} is unknown").into())
            })
            .transpose()
    }

    pub fn thinking(&self) -> Result<Option<Thinking>> {
        self.thinking
            .as_deref()
            .map(host::parse_thinking)
            .transpose()
    }

    pub fn tier(&self) -> Result<Option<ServiceTier>> {
        self.tier.as_deref().map(host::parse_tier).transpose()
    }
}

pub fn path(home: &Path) -> PathBuf {
    home.join(FILE)
}

pub async fn load(home: &Path) -> Result<Defaults> {
    let path = path(home);
    nanocode_platform::spawn_blocking(move || read(&path)).await?
}

fn read(path: &Path) -> Result<Defaults> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|error| {
            format!(
                "{}: {error}; fix it or run 'nc default reset'",
                path.display()
            )
            .into()
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Defaults::default()),
        Err(error) => Err(format!("{}: {error}", path.display()).into()),
    }
}

async fn save(home: &Path, defaults: Defaults) -> Result<()> {
    let home = home.to_path_buf();
    nanocode_platform::spawn_blocking(move || write(&home, &defaults)).await?
}

fn write(home: &Path, defaults: &Defaults) -> Result<()> {
    let path = path(home);
    if *defaults == Defaults::default() {
        return match std::fs::remove_file(&path) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                Err(format!("{}: {error}", path.display()).into())
            }
            _ => Ok(()),
        };
    }
    let mut bytes = serde_json::to_vec_pretty(defaults)?;
    bytes.push(b'\n');
    let temporary = home.join(format!(".{FILE}.{}", std::process::id()));
    let written = std::fs::create_dir_all(home)
        .and_then(|()| {
            let mut file = std::fs::File::create(&temporary)?;
            std::io::Write::write_all(&mut file, &bytes)?;
            file.sync_all()
        })
        .and_then(|()| std::fs::rename(&temporary, &path));
    if written.is_err() {
        drop(std::fs::remove_file(&temporary));
    }
    written.map_err(|error| format!("{}: {error}", path.display()).into())
}

pub async fn command(words: Vec<String>, clients: ModelClients, home: PathBuf) -> Result<()> {
    let report = run(&words, &clients, &home).await?;
    print!("{report}");
    Ok(())
}

pub async fn run(words: &[String], clients: &ModelClients, home: &Path) -> Result<String> {
    if let Some(tempo) = clients.tempo_models() {
        tempo.load().await;
    }
    clients.listed().await?;
    let current = load(home).await?;
    let mut report = String::new();
    let shown = if words.is_empty() {
        current
    } else {
        let (next, dropped) = edit(words, &current, clients)?;
        for note in dropped {
            writeln!(report, "{note}")?;
        }
        save(home, next.clone()).await?;
        next
    };
    show(&shown, clients, home, &mut report)?;
    Ok(report)
}

#[derive(Default)]
struct Update {
    harness: Option<HarnessArg>,
    model: Option<(host::Route, ModelSelection)>,
    thinking: Option<Thinking>,
    tier: Option<ServiceTier>,
}

fn edit(
    words: &[String],
    current: &Defaults,
    clients: &ModelClients,
) -> Result<(Defaults, Vec<String>)> {
    let words = words
        .iter()
        .flat_map(|word| word.split_whitespace())
        .collect::<Vec<_>>();
    let Some((first, rest)) = words.split_first() else {
        return Ok((current.clone(), Vec::new()));
    };
    let first = first.to_ascii_lowercase();
    match (first.as_str(), rest) {
        ("reset", []) => return Ok((Defaults::default(), Vec::new())),
        ("unset", []) => return Err(format!("name what to unset: {KEYS}").into()),
        ("unset", keys) => {
            let mut next = current.clone();
            for key in keys {
                match key.to_ascii_lowercase().as_str() {
                    "harness" => {
                        next.harness = None;
                        next.model = None;
                    }
                    "model" => next.model = None,
                    "thinking" => next.thinking = None,
                    "tier" => next.tier = None,
                    other => {
                        return Err(
                            format!("unknown default {other:?}; expected one of {KEYS}").into()
                        );
                    }
                }
            }
            return Ok((next, Vec::new()));
        }
        _ => {}
    }
    let mut update = Update::default();
    match (first.as_str(), rest) {
        ("harness", [value]) => update.harness = Some(harness(value)?),
        ("model", [value]) => update.model = Some(model(value, clients)?),
        ("thinking", [value]) => update.thinking = Some(host::parse_thinking(value)?),
        ("tier", [value]) => update.tier = Some(tier(value)?),
        (key @ ("harness" | "model" | "thinking" | "tier"), _) => {
            return Err(format!("nc default {key} takes one value").into());
        }
        _ => {
            for word in &words {
                classify(&mut update, word, clients)?;
            }
        }
    }
    merge(update, current, clients)
}

fn harness(value: &str) -> Result<HarnessArg> {
    HarnessArg::named(value).ok_or_else(|| {
        format!("unknown harness {value:?}; expected codex (or openai, chatgpt), claude (or anthropic) or antigravity (or google, gemini)").into()
    })
}

fn tier(value: &str) -> Result<ServiceTier> {
    host::parse_tier(&value.to_ascii_lowercase())
        .map_err(|_| format!("unknown service tier {value:?}; expected one of {TIER_NAMES}").into())
}

fn model(value: &str, clients: &ModelClients) -> Result<(host::Route, ModelSelection)> {
    if value.to_ascii_lowercase().starts_with("gemini-") {
        return Ok((
            host::Route::Native,
            nanocode::antigravity::AntigravityHarness::selection(value),
        ));
    }
    clients.resolve(value)
}

fn classify(update: &mut Update, word: &str, clients: &ModelClients) -> Result<()> {
    fn once<T>(slot: &mut Option<T>, value: T, kind: &str, word: &str) -> Result<()> {
        if slot.is_some() {
            return Err(format!("{word:?} is a second {kind}; give one").into());
        }
        *slot = Some(value);
        Ok(())
    }
    if let Some(value) = HarnessArg::named(word) {
        return once(&mut update.harness, value, "harness", word);
    }
    if let Ok(value) = host::parse_thinking(word) {
        return once(&mut update.thinking, value, "thinking level", word);
    }
    if let Ok(value) = tier(word) {
        return once(&mut update.tier, value, "service tier", word);
    }
    match model(word, clients) {
        Ok(value) => once(&mut update.model, value, "model", word),
        Err(error) => {
            let thinking = Thinking::ALL.map(Thinking::as_str).join(", ");
            Err(format!(
                "{word:?} is not a harness (codex, claude, antigravity, or openai, chatgpt, anthropic, google, gemini), a thinking level ({thinking}), a service tier ({TIER_NAMES}) or a model: {error}"
            )
            .into())
        }
    }
}

fn merge(
    update: Update,
    current: &Defaults,
    clients: &ModelClients,
) -> Result<(Defaults, Vec<String>)> {
    let mut next = current.clone();
    let chosen = match (update.model, update.harness) {
        (Some((route, model)), Some(harness)) if host::follows_harness(route, &model) => {
            Some((route, host::on_harness(&model, &family(harness))))
        }
        (model, _) => model,
    };
    match (chosen, update.harness) {
        (Some((route, model)), Some(harness)) if family(harness) != model.family() => {
            return Err(format!(
                "model {} does not belong to the {} harness but to {}; 'nc models' lists each harness's models",
                route.label(&model),
                harness.as_str(),
                harness_of(&model.family()).as_str(),
            )
            .into());
        }
        (Some((route, model)), _) => {
            next.model = Some(route.label(&model));
            next.harness = Some(harness_of(&model.family()).as_str().to_owned());
        }
        (None, Some(harness)) => {
            let keeps = current
                .model
                .as_deref()
                .and_then(|saved| model(saved, clients).ok())
                .is_some_and(|(_, model)| model.family() == family(harness));
            if !keeps {
                next.model = None;
            }
            next.harness = Some(harness.as_str().to_owned());
        }
        (None, None) => {}
    }
    let (_, effective) = effective_model(&next, clients)?;
    let capabilities = effective.capabilities(nanocode::ModelTransport::Native);
    let mut dropped = Vec::new();
    match update.thinking {
        Some(thinking) => {
            capabilities.check_thinking(thinking)?;
            next.thinking = Some(thinking.as_str().to_owned());
        }
        None => {
            if let Some(thinking) = next.thinking()?
                && !capabilities.supports_thinking(thinking)
            {
                dropped.push(format!(
                    "cleared the saved {thinking} thinking: {effective} does not support it"
                ));
                next.thinking = None;
            }
        }
    }
    match update.tier {
        Some(tier) => {
            capabilities.check_service_tier(tier)?;
            next.tier = Some(tier.as_str().to_owned());
        }
        None => {
            if let Some(tier) = next.tier()?
                && !capabilities.supports_service_tier(tier)
            {
                dropped.push(format!(
                    "cleared the saved {} tier: {effective} does not support it",
                    tier.as_str()
                ));
                next.tier = None;
            }
        }
    }
    Ok((next, dropped))
}

fn harness_of(family: &nanocode::HarnessFamily) -> HarnessArg {
    if *family == nanocode::HarnessFamily::Claude {
        HarnessArg::Claude
    } else if family.as_str() == nanocode::antigravity::FAMILY {
        HarnessArg::Antigravity
    } else {
        HarnessArg::Codex
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Source {
    Saved,
    Harness,
    BuiltIn,
}

impl Source {
    const fn label(self) -> &'static str {
        match self {
            Self::Saved => "saved",
            Self::Harness => "the saved harness's default",
            Self::BuiltIn => "built-in",
        }
    }
}

fn effective_model(
    defaults: &Defaults,
    clients: &ModelClients,
) -> Result<(Source, ModelSelection)> {
    if let Some(saved) = &defaults.model {
        return Ok((Source::Saved, model(saved, clients)?.1));
    }
    Ok(match defaults.harness()? {
        Some(HarnessArg::Antigravity) => (
            Source::Harness,
            nanocode::antigravity::AntigravityHarness::selection(
                nanocode::antigravity::DEFAULT_MODEL,
            ),
        ),
        Some(harness) => (Source::Harness, clients.default_model(&family(harness))?),
        None => (
            Source::BuiltIn,
            match clients.codex_models().transpose()?.and_then(<[_]>::first) {
                Some(model) => ModelSelection::Codex(*model),
                None => nanocode::HarnessFamily::Claude.default_model(),
            },
        ),
    })
}

fn show(
    defaults: &Defaults,
    clients: &ModelClients,
    home: &Path,
    report: &mut String,
) -> Result<()> {
    let (source, model) = effective_model(defaults, clients)?;
    let capabilities = model.capabilities(nanocode::ModelTransport::Native);
    let harness = match defaults.harness()? {
        Some(harness) => (harness, Source::Saved),
        None => (harness_of(&model.family()), Source::BuiltIn),
    };
    let label = defaults.model.clone().unwrap_or_else(|| model.to_string());
    let thinking = match defaults.thinking()? {
        Some(thinking) => (thinking, Source::Saved),
        None if source == Source::BuiltIn && capabilities.supports_thinking(Thinking::Xhigh) => {
            (Thinking::Xhigh, Source::BuiltIn)
        }
        None => (model.default_thinking(), Source::BuiltIn),
    };
    let tier = match defaults.tier()? {
        Some(tier) => (tier, Source::Saved),
        None => (ServiceTier::Standard, Source::BuiltIn),
    };
    let rows = [
        ("harness", harness.0.as_str().to_owned(), harness.1),
        ("model", label, source),
        ("thinking", thinking.0.as_str().to_owned(), thinking.1),
        ("tier", tier.0.as_str().to_owned(), tier.1),
    ];
    let width = rows.iter().map(|row| row.1.len()).max().unwrap_or_default();
    for (key, value, source) in rows {
        writeln!(report, "{key:<9}{value:<width$}  {}", source.label())?;
    }
    let state = if *defaults == Defaults::default() {
        " (nothing saved)"
    } else {
        ""
    };
    writeln!(report, "file     {}{state}", path(home).display())?;
    if let Ok(model) = std::env::var("NANOCODE_MODEL")
        && !model.trim().is_empty()
    {
        writeln!(
            report,
            "NANOCODE_MODEL={model} overrides the saved harness and model"
        )?;
    }
    Ok(())
}
