use std::fmt;

const WORKSPACE: &str = "workspace";
const HOSTED: &str = "hosted";
const POOL: &str = "pool";
const NATIVE: &str = "native";
const MAC: &str = "mac";
const SEPARATOR: char = ':';

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum HandRef {
    Hosted,
    Pool(Box<str>),
    Native(Box<str>),
    Mac,
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum Target {
    Workspace,
    Hand(HandRef),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TargetError {
    Unknown(Box<str>),
    MissingName(&'static str),
    UnexpectedName(&'static str),
    NotAHand,
}

impl fmt::Display for TargetError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unknown(wire) => write!(
                formatter,
                "`{wire}` is not a target: use workspace, hosted, pool:<id>, native:<name> or mac"
            ),
            Self::MissingName(kind) => write!(formatter, "`{kind}:` needs a name after the colon"),
            Self::UnexpectedName(kind) => write!(formatter, "`{kind}` takes no name"),
            Self::NotAHand => formatter.write_str("the workspace is not a Hand"),
        }
    }
}

impl std::error::Error for TargetError {}

impl HandRef {
    pub fn parse(wire: &str) -> Result<Self, TargetError> {
        match Target::parse(wire)? {
            Target::Hand(hand) => Ok(hand),
            Target::Workspace => Err(TargetError::NotAHand),
        }
    }
}

impl Target {
    pub fn parse(wire: &str) -> Result<Self, TargetError> {
        let (kind, name) = match wire.split_once(SEPARATOR) {
            Some((kind, name)) => (kind, Some(name)),
            None => (wire, None),
        };
        match (kind, name) {
            (WORKSPACE, None) => Ok(Self::Workspace),
            (HOSTED, None) => Ok(Self::Hand(HandRef::Hosted)),
            (MAC, None) => Ok(Self::Hand(HandRef::Mac)),
            (WORKSPACE, Some(_)) => Err(TargetError::UnexpectedName(WORKSPACE)),
            (HOSTED, Some(_)) => Err(TargetError::UnexpectedName(HOSTED)),
            (MAC, Some(_)) => Err(TargetError::UnexpectedName(MAC)),
            (POOL, Some(name)) => named(POOL, name).map(|name| Self::Hand(HandRef::Pool(name))),
            (NATIVE, Some(name)) => {
                named(NATIVE, name).map(|name| Self::Hand(HandRef::Native(name)))
            }
            (POOL, None) => Err(TargetError::MissingName(POOL)),
            (NATIVE, None) => Err(TargetError::MissingName(NATIVE)),
            _ => Err(TargetError::Unknown(wire.into())),
        }
    }
}

fn named(kind: &'static str, name: &str) -> Result<Box<str>, TargetError> {
    if name.is_empty() {
        return Err(TargetError::MissingName(kind));
    }
    Ok(name.into())
}

impl fmt::Display for HandRef {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Hosted => formatter.write_str(HOSTED),
            Self::Pool(id) => write!(formatter, "{POOL}{SEPARATOR}{id}"),
            Self::Native(name) => write!(formatter, "{NATIVE}{SEPARATOR}{name}"),
            Self::Mac => formatter.write_str(MAC),
        }
    }
}

impl fmt::Display for Target {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Workspace => formatter.write_str(WORKSPACE),
            Self::Hand(hand) => hand.fmt(formatter),
        }
    }
}
