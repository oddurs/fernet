//! Shell completion: `fernet completions <shell>` prints a script that asks
//! `fernet complete <word>` for candidates, so keys after `#` and versions
//! and marks after `@` complete from the real file and store.

use std::ffi::OsStr;

use fernet_fs_core::{EntryKind, Fernet, Resolved};
use fernet_fs_path::DayName;

/// Subcommands, for the first word.
pub(crate) const COMMANDS: &str =
    "init watch mount cat ls write core log mark diff compact verify reindex completions";

/// The completion script for `shell`.
pub(crate) fn script(shell: &str) -> Option<String> {
    Some(match shell {
        "bash" => format!(
            r#"# fernet completion for bash. Add to ~/.bashrc:  eval "$(fernet completions bash)"
_fernet() {{
    # The whole word, not split at `@` or `:` (both in COMP_WORDBREAKS).
    local line="${{COMP_LINE:0:COMP_POINT}}"
    local cur="${{line##*[[:space:]]}}"
    if [ "$COMP_CWORD" -eq 1 ]; then
        COMPREPLY=($(compgen -W "{COMMANDS}" -- "$cur"))
        return
    fi
    local IFS=$'\n'
    local candidates=($(fernet complete -- "$cur" 2>/dev/null))
    # bash replaces only the text after the last word break: drop that prefix.
    local breaks="${{COMP_WORDBREAKS// /}}"
    local prefix="${{cur%"${{cur##*[$breaks]}}"}}"
    COMPREPLY=("${{candidates[@]#"$prefix"}}")
    compopt -o nospace 2>/dev/null
}}
complete -F _fernet fernet
"#
        ),
        "zsh" => format!(
            r#"#compdef fernet
# fernet completion for zsh. Add to ~/.zshrc:  eval "$(fernet completions zsh)"
# With EXTENDED_GLOB, quote fernet paths or `alias fernet='noglob fernet'`.
_fernet() {{
    if (( CURRENT == 2 )); then
        compadd -- {COMMANDS}
        return
    fi
    local -a candidates
    candidates=("${{(@f)$(fernet complete -- "${{words[CURRENT]}}" 2>/dev/null)}}")
    compadd -Q -S '' -- $candidates
}}
compdef _fernet fernet
"#
        ),
        "fish" => format!(
            r"# fernet completion for fish. Save as ~/.config/fish/completions/fernet.fish
complete -c fernet -f -n '__fish_use_subcommand' -a '{COMMANDS}'
complete -c fernet -f -n 'not __fish_use_subcommand' -a '(fernet complete -- (commandline -ct))'
"
        ),
        _ => return None,
    })
}

/// Candidates for a partly typed fernet path.
pub(crate) fn candidates(f: &Fernet, cwd: &std::path::Path, word: &str) -> Vec<String> {
    let (dir, prefix) = match word.rfind('/') {
        Some(i) => (&word[..=i], &word[i + 1..]),
        None => ("", word),
    };
    let mut out = Vec::new();
    let base = if dir.is_empty() { "." } else { dir };
    let Ok(parent) = f.resolve_user_path(cwd, OsStr::new(base)) else {
        return out;
    };
    // Real names (and keys) first: they win over lenses.
    let lens_base = prefix.rfind('@').map(|at| &prefix[..at]);
    let mut real_kind = None;
    if let Ok(entries) = f.list(&parent) {
        for e in entries {
            let name = e.name.to_string_lossy();
            if lens_base == Some(&*name) {
                real_kind = Some(e.kind);
            }
            if name.starts_with(prefix) {
                let slash = if e.kind == EntryKind::Dir { "/" } else { "" };
                out.push(format!("{dir}{name}{slash}"));
            }
        }
    }
    // Inside a file, `@` and `#` are part of a key, never lenses.
    if matches!(parent, Resolved::Inside { .. }) {
        return out;
    }
    // `name@…`: time specs, marks and, for files, the version listing.
    if let Some(at) = prefix.rfind('@') {
        let (name, spec) = (&prefix[..at], &prefix[at + 1..]);
        let file = real_kind != Some(EntryKind::Dir);
        let mut specs: Vec<String> = Vec::new();
        if file {
            specs.extend(["/".into(), "~1".into()]);
        }
        specs.extend(["first".into(), "-1h".into(), "-1d".into()]);
        specs.extend(DayName::ALL.iter().map(|d| d.as_str().to_owned()));
        if let Some(store) = f.store() {
            specs.extend(store.marks().unwrap_or_default().into_keys());
        }
        out.extend(
            specs
                .into_iter()
                .filter(|s| s.starts_with(spec))
                .map(|s| format!("{dir}{name}@{s}")),
        );
        return out;
    }
    // `name#`: open a structured file.
    if let Some(name) = prefix.strip_suffix('#') {
        let path = format!("{dir}{prefix}");
        if let Ok(Resolved::Inside { node, .. }) = f.resolve_user_path(cwd, OsStr::new(&path))
            && matches!(
                node,
                fernet_fs_core::NodeKind::Object | fernet_fs_core::NodeKind::Array
            )
        {
            out.push(format!("{dir}{name}#/"));
        }
    }
    out
}
