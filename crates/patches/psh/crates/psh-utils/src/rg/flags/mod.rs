// Copied from ripgrep (https://github.com/BurntSushi/ripgrep, commit 91f5d22),
// crates/core/flags/mod.rs, with the psh patches marked `psh:` (D1133):
// Copyright (c) 2015 Andrew Gallant. Licensed under the MIT License (see NOTICE).

/*!
Defines ripgrep's command line interface.

This modules deals with everything involving ripgrep's flags and positional
arguments. This includes generating shell completions, `--help` output and even
ripgrep's man page. It's also responsible for parsing and validating every
flag (including reading ripgrep's config file), and manages the contact points
between these flags and ripgrep's cast of supporting libraries. For example,
once [`HiArgs`] has been created, it knows how to create a multi threaded
recursive directory traverser.
*/
use crate::rg::anyhow;
use std::ffi::OsString;

pub(crate) use crate::rg::flags::config::Source as ConfigSource;
pub(crate) use crate::rg::flags::{
    doc::{
        help::{
            generate_long as generate_help_long,
            generate_short as generate_help_short,
        },
        version::{
            generate_long as generate_version_long,
            generate_pcre2 as generate_version_pcre2,
            generate_short as generate_version_short,
        },
    },
    hiargs::HiArgs,
    lowargs::{
        EngineChoice, LowArgs, Mode, PatternSource, SearchMode, SpecialMode,
    },
    parse::{ParseResult, parse_low},
};

mod config;
mod defs;
mod doc;
mod hiargs;
mod lowargs;
mod parse;

/// psh: the definition of one of ripgrep's flags, as data (ripgrep's `Flag` trait
/// with one unit struct per flag): its long name, an optional short name, aliases
/// and negation, its documentation and how a value updates `LowArgs`
/// (`-V/--version` and `-h/--help` are handled by the parser and do nothing there).
#[derive(Debug)]
struct Flag {
    switch: bool,
    short: Option<u8>,
    long: &'static str,
    aliases: &'static [&'static str],
    negated: Option<&'static str>,
    var: Option<&'static str>,
    category: Category,
    doc_short: &'static str,
    doc_long: &'static str,
    /// A long documentation built when asked (`--hyperlink-format`'s).
    doc_fn: Option<fn() -> &'static str>,
    update: fn(FlagValue, &mut LowArgs) -> anyhow::Result<()>,
}

/// The category that a flag belongs to.
///
/// Categories are used to organize flags into "logical" groups in the
/// generated documentation.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, PartialOrd, Ord)]
enum Category {
    /// Flags related to how ripgrep reads its input. Its "input" generally
    /// consists of the patterns it is trying to match and the haystacks it is
    /// trying to search.
    Input,
    /// Flags related to the operation of the search itself. For example,
    /// whether case insensitive matching is enabled.
    Search,
    /// Flags related to how ripgrep filters haystacks. For example, whether
    /// to respect gitignore files or not.
    Filter,
    /// Flags related to how ripgrep shows its search results. For example,
    /// whether to show line numbers or not.
    Output,
    /// Flags related to changing ripgrep's output at a more fundamental level.
    /// For example, flags like `--count` suppress printing of individual
    /// lines, and instead just print the total count of matches for each file
    /// searched.
    OutputModes,
    /// Flags related to logging behavior such as emitting non-fatal error
    /// messages or printing search statistics.
    Logging,
    /// Other behaviors not related to ripgrep's core functionality. For
    /// example, printing the file type globbing rules, or printing the list
    /// of files ripgrep would search without actually searching them.
    OtherBehaviors,
}

impl Category {
    /// Returns a string representation of this category.
    ///
    /// This string is the name of the variable used in various templates for
    /// generated documentation. This name can be used for interpolation.
    fn as_str(&self) -> &'static str {
        match *self {
            Category::Input => "input",
            Category::Search => "search",
            Category::Filter => "filter",
            Category::Output => "output",
            Category::OutputModes => "output-modes",
            Category::Logging => "logging",
            Category::OtherBehaviors => "other-behaviors",
        }
    }
}

/// Represents a value parsed from the command line.
///
/// This doesn't include the corresponding flag, but values come in one of
/// two forms: a switch (on or off) or an arbitrary value.
///
/// Note that the CLI doesn't directly support negated switches. For example,
/// you can'd do anything like `-n=false` or any of that nonsense. Instead,
/// the CLI parser knows about which flag names are negations and which aren't
/// (courtesy of the `Flag` trait). If a flag given is known as a negation,
/// then a `FlagValue::Switch(false)` value is passed into `Flag::update`.
#[derive(Debug)]
enum FlagValue {
    /// A flag that is either on or off.
    Switch(bool),
    /// A flag that comes with an arbitrary user value.
    Value(OsString),
}

impl FlagValue {
    /// Return the yes or no value of this switch.
    ///
    /// If this flag value is not a switch, then this panics.
    ///
    /// This is useful when writing the implementation of `Flag::update`.
    /// namely, callers usually know whether a switch or a value is expected.
    /// If a flag is something different, then it indicates a bug, and thus a
    /// panic is acceptable.
    fn unwrap_switch(self) -> bool {
        match self {
            FlagValue::Switch(yes) => yes,
            FlagValue::Value(_) => {
                unreachable!("got flag value but expected switch")
            }
        }
    }

    /// Return the user provided value of this flag.
    ///
    /// If this flag is a switch, then this panics.
    ///
    /// This is useful when writing the implementation of `Flag::update`.
    /// namely, callers usually know whether a switch or a value is expected.
    /// If a flag is something different, then it indicates a bug, and thus a
    /// panic is acceptable.
    fn unwrap_value(self) -> OsString {
        match self {
            FlagValue::Switch(_) => {
                unreachable!("got switch but expected flag value")
            }
            FlagValue::Value(v) => v,
        }
    }
}
