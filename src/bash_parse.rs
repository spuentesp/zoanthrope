//! Heuristic extraction of file-mutation targets from shell commands.
//!
//! Claude Code's `Edit`/`Write` tools carry a `file_path` and are
//! compare-and-swap / occ-checked by the host. `Bash` mutations —
//! `cat > FILE`, `sed -i`, `tee`, `cp`/`mv`, `python -c` writes — have
//! neither: a whole-file shell write can silently clobber a concurrent
//! agent's work. The PreToolUse guard uses this parser to claim every
//! workspace file a Bash command is about to touch, so the claim flow
//! covers the same ground as `Edit`/`Write`.
//!
//! The parse is deliberately conservative and deliberately heuristic:
//! an **allowlist of mutation idioms**, not a shell interpreter. When a
//! command's mutations cannot be parsed, nothing is claimed here and
//! the post-hoc watch hook still fingerprints the change and reports an
//! `unclaimed-mutation` line. Known limits (by design):
//!
//! * only the idioms below are recognized — anything else (`dd of=…`,
//!   `truncate`, `install`, `rsync`, editors, …) is invisible here and
//!   falls to the watch hook;
//! * no `eval`, no `$VAR` expansion, no command substitution: paths
//!   must be literal words in the command text;
//! * redirect targets are whatever word follows `>`/`>>`/`&>`; word
//!   splitting honors simple quoting but not shell concatenation
//!   (`a"b"c`);
//! * `cp`/`mv` treat the final argument as the target (or the `-t`
//!   directory); `sed -i`/`sed -i.bak` report the operand files (the
//!   backup file is not reported);
//! * `python`/`perl` `--command` bodies are scanned for obvious write
//!   sinks (`open(path, "w"-ish)`, perl three-arg / glued-mode `open`,
//!   `Path(...).write_text/write_bytes`) — string literals only, no
//!   computed paths;
//! * nested `sh -c '…'` / `bash -c '…'` scripts and heredoc interpreter
//!   scripts (`python3 - <<'PY'`) are re-parsed recursively; the
//!   heredoc *payload* of a plain `cat > f <<EOF` is not.

use std::path::{Path, PathBuf};

/// Extract the file paths a shell command would mutate.
///
/// Returns path words exactly as written (relative or absolute, quotes
/// stripped, deduplicated in first-seen order). Filtering to a
/// workspace is [`workspace_targets`]'s job.
pub fn mutation_targets(cmd: &str) -> Vec<String> {
    let mut out = Vec::new();
    for seg in split_commands(cmd) {
        scan_segment(&seg, &mut out);
    }
    dedup(out)
}

/// [`mutation_targets`] resolved against `cwd` and restricted to files
/// under `ws_root`. Paths that escape the workspace (e.g. `/etc/hosts`)
/// are dropped: the guard only arbitrates workspace files. A relative
/// `ws_root` resolves against the process cwd, never against `cwd`
/// (which is the command's own working directory).
pub fn workspace_targets(cmd: &str, cwd: &Path, ws_root: &Path) -> Vec<PathBuf> {
    let root = against_process_cwd(ws_root);
    let base = against_process_cwd(cwd);
    let mut out = Vec::new();
    for raw in mutation_targets(cmd) {
        let p = if Path::new(&raw).is_absolute() {
            normalize(Path::new(&raw))
        } else {
            normalize(&base.join(&raw))
        };
        if p.starts_with(&root) && p != root {
            out.push(p);
        }
    }
    dedup_paths(out)
}

fn against_process_cwd(p: &Path) -> PathBuf {
    if p.is_absolute() {
        normalize(p)
    } else {
        normalize(&std::env::current_dir().unwrap_or_default().join(p))
    }
}

/// Lexical `.`/`..` normalization — enough for hook-time path identity.
fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in p.components() {
        match comp {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn dedup(items: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for it in items {
        if seen.insert(it.clone()) {
            out.push(it);
        }
    }
    out
}

fn dedup_paths(items: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for it in items {
        if seen.insert(it.clone()) {
            out.push(it);
        }
    }
    out
}

// ─── command splitting ──────────────────────────────────────────────

/// One pipeline stage / list element: its command text plus the body
/// of any heredoc attached to it.
struct Segment {
    text: String,
    heredoc: Option<String>,
}

/// Split a shell command into segments on unquoted `;`, `&&`, `||`,
/// `|`, `|&` and newlines, capturing `<<DELIM` heredoc bodies.
fn split_commands(cmd: &str) -> Vec<Segment> {
    let mut segs: Vec<Segment> = Vec::new();
    let mut cur = String::new();
    let chars: Vec<char> = cmd.chars().collect();
    let mut i = 0usize;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '\'' | '"' => {
                let quote = c;
                cur.push(c);
                i += 1;
                while i < chars.len() {
                    cur.push(chars[i]);
                    if chars[i] == quote {
                        i += 1;
                        break;
                    }
                    i += 1;
                }
            }
            ';' | '\n' => {
                segs.push(Segment {
                    text: std::mem::take(&mut cur),
                    heredoc: None,
                });
                i += 1;
            }
            '|' => {
                if i + 1 < chars.len() && (chars[i + 1] == '|' || chars[i + 1] == '&') {
                    i += 2;
                } else {
                    i += 1;
                }
                segs.push(Segment {
                    text: std::mem::take(&mut cur),
                    heredoc: None,
                });
            }
            '&' if i + 1 < chars.len() && chars[i + 1] == '&' => {
                i += 2;
                segs.push(Segment {
                    text: std::mem::take(&mut cur),
                    heredoc: None,
                });
            }
            '<' if i + 2 < chars.len() && chars[i + 1] == '<' && chars[i + 2] == '<' => {
                // here-string `<<< value` — not a file write; keep text
                cur.push_str("<<<");
                i += 3;
            }
            '<' if i + 1 < chars.len() && chars[i + 1] == '<' => {
                // heredoc `<<DELIM` / `<<-DELIM`
                i += 2;
                while i < chars.len() && matches!(chars[i], ' ' | '\t' | '-') {
                    i += 1;
                }
                let mut delim = String::new();
                let mut quote: Option<char> = None;
                while i < chars.len() && chars[i] != '\n' {
                    let c2 = chars[i];
                    i += 1;
                    match quote {
                        Some(q) if c2 == q => quote = None,
                        Some(_) => delim.push(c2),
                        None if c2 == '\'' || c2 == '"' => quote = Some(c2),
                        None if c2 == ' ' || c2 == '\t' => {}
                        None => delim.push(c2),
                    }
                }
                if i < chars.len() && chars[i] == '\n' {
                    i += 1;
                }
                let mut body = String::new();
                let mut line = String::new();
                let mut closed = false;
                while i < chars.len() {
                    line.push(chars[i]);
                    if chars[i] == '\n' {
                        if !delim.is_empty() && line.trim().trim_start_matches(['\t']) == delim {
                            closed = true;
                            i += 1;
                            break;
                        }
                        body.push_str(&line);
                        line.clear();
                    }
                    i += 1;
                }
                if !closed && !line.is_empty() {
                    body.push_str(&line);
                }
                segs.push(Segment {
                    text: std::mem::take(&mut cur),
                    heredoc: if body.is_empty() { None } else { Some(body) },
                });
            }
            _ => {
                cur.push(c);
                i += 1;
            }
        }
    }
    if !cur.trim().is_empty() {
        segs.push(Segment {
            text: cur,
            heredoc: None,
        });
    }
    segs
}

// ─── per-segment scanning ───────────────────────────────────────────

fn scan_segment(seg: &Segment, out: &mut Vec<String>) {
    let text = seg.text.as_str();
    scan_redirects(text, out);
    let words = split_words(text);
    let Some(head) = words.first() else { return };
    match head.as_str() {
        "sed" => sed_targets(&words, out),
        "tee" => tee_targets(&words, out),
        "cp" | "mv" | "rename" => cp_mv_targets(&words, out),
        "python" | "python2" | "python3" => {
            interpreter_sinks(&words, &["-c", "--command"], seg, out)
        }
        "perl" => interpreter_sinks(&words, &["-e", "-E"], seg, out),
        "ruby" => interpreter_sinks(&words, &["-e"], seg, out),
        "sh" | "bash" | "zsh" | "dash" | "ksh" => {
            if let Some(inner) = inline_code(&words, &["-c"]) {
                out.extend(mutation_targets(&inner));
            }
        }
        _ => {}
    }
}

/// Interpreter write sinks: inline `-c`/`-e` code, or a heredoc script.
fn interpreter_sinks(words: &[String], flags: &[&str], seg: &Segment, out: &mut Vec<String>) {
    if let Some(code) = inline_code(words, flags) {
        code_write_sinks(&code, out);
    } else if words.iter().skip(1).any(|w| w == "-") {
        if let Some(body) = &seg.heredoc {
            code_write_sinks(body, out);
        }
    }
}

/// Quoting-aware scan for `> FILE` / `>> FILE` / `&> FILE` redirections.
/// Skips heredocs/here-strings (`<<`, `<<<`), fd duplications (`>&2`)
/// and process substitution (`>(`).
fn scan_redirects(text: &str, out: &mut Vec<String>) {
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0usize;
    let mut quote: Option<char> = None;
    while i < chars.len() {
        let c = chars[i];
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                }
                i += 1;
                continue;
            }
            None if c == '\'' || c == '"' => {
                quote = Some(c);
                i += 1;
                continue;
            }
            None => {}
        }
        if c == '<' {
            i += 1;
            continue;
        }
        if c == '>' {
            let prev = if i > 0 { chars[i - 1] } else { ' ' };
            let next = chars.get(i + 1).copied().unwrap_or(' ');
            // `>&` fd duplication and `>(` process substitution
            if next == '&' || next == '(' {
                i += 1;
                continue;
            }
            // `&&>` is not a redirection operator
            if prev == '&' && i >= 2 && chars[i - 2] == '&' {
                i += 1;
                continue;
            }
            let mut j = i + 1;
            if next == '>' {
                j += 1;
            }
            while j < chars.len() && chars[j].is_whitespace() {
                j += 1;
            }
            if j < chars.len() {
                if let Some((word, end)) = word_at(&chars, j) {
                    if !word.is_empty() {
                        out.push(word);
                    }
                    i = end;
                    continue;
                }
            }
            i = j;
            continue;
        }
        i += 1;
    }
}

/// Read one shell word starting at `start`, honoring quotes. Returns
/// the unquoted word and the raw end index.
fn word_at(chars: &[char], start: usize) -> Option<(String, usize)> {
    let mut word = String::new();
    let mut i = start;
    let mut saw = false;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            break;
        }
        saw = true;
        if c == '\'' || c == '"' {
            let q = c;
            i += 1;
            while i < chars.len() && chars[i] != q {
                word.push(chars[i]);
                i += 1;
            }
            i += 1; // closing quote
            continue;
        }
        if c == '\\' {
            i += 1;
            if i < chars.len() {
                word.push(chars[i]);
                i += 1;
            }
            continue;
        }
        word.push(c);
        i += 1;
    }
    if saw {
        Some((word, i))
    } else {
        None
    }
}

/// Split a segment into shell words (quotes stripped). Not a full
/// shell lexer — enough to dispatch on the command word and walk
/// simple option arguments.
fn split_words(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut words = Vec::new();
    let mut i = 0usize;
    while i < chars.len() {
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        if i >= chars.len() {
            break;
        }
        match word_at(&chars, i) {
            Some((w, end)) => {
                words.push(w);
                i = end;
            }
            None => i += 1,
        }
    }
    words
}

/// `sed` with `-i` / `--in-place`: the file operands are the mutated
/// files (the script operand is skipped when no `-e`/`-f` is given).
fn sed_targets(words: &[String], out: &mut Vec<String>) {
    if !words.iter().any(|w| is_sed_in_place(w)) {
        return;
    }
    let mut has_script_opt = false;
    let mut operands: Vec<String> = Vec::new();
    let mut i = 1;
    while i < words.len() {
        let w = &words[i];
        if w == "--" {
            operands.extend_from_slice(&words[i + 1..]);
            break;
        }
        if w.starts_with("--") {
            if w == "--expression" || w == "--file" {
                has_script_opt = true;
                i += 2;
                continue;
            }
            if w.starts_with("--expression=") || w.starts_with("--file=") {
                has_script_opt = true;
            }
            i += 1;
            continue;
        }
        if w.starts_with('-') {
            if w == "-e" || w == "-f" {
                has_script_opt = true;
                i += 2;
                continue;
            }
            i += 1;
            continue;
        }
        operands.push(w.clone());
        i += 1;
    }
    let skip = usize::from(!has_script_opt && !operands.is_empty());
    out.extend(operands.into_iter().skip(skip));
}

fn is_sed_in_place(w: &str) -> bool {
    w == "-i" || w == "--in-place" || (w.starts_with("-i") && !w.starts_with("-i-") && w.len() > 2)
}

/// `tee [-a] FILE…` — every non-option argument is an output file.
fn tee_targets(words: &[String], out: &mut Vec<String>) {
    let mut rest = words.iter().skip(1).peekable();
    while let Some(w) = rest.next() {
        if w == "--" {
            for f in rest.by_ref() {
                out.push(f.clone());
            }
            return;
        }
        if w.starts_with('-') && w.len() > 1 {
            continue;
        }
        out.push(w.clone());
    }
}

/// `cp SRC… DST` / `mv SRC… DST`: the last argument is the mutated
/// target (or the `-t DIR` destination directory).
fn cp_mv_targets(words: &[String], out: &mut Vec<String>) {
    let rest = &words[1..];
    for (i, w) in rest.iter().enumerate() {
        if w == "-t" || w == "--target-directory" {
            if let Some(t) = rest.get(i + 1) {
                out.push(t.clone());
            }
            return;
        }
        if let Some(t) = w.strip_prefix("--target-directory=") {
            out.push(t.to_string());
            return;
        }
    }
    // last operand (a flag cluster is never the destination)
    if let Some(last) = rest
        .iter()
        .rev()
        .find(|w| !w.starts_with('-') || w.as_str() == "-")
    {
        out.push(last.clone());
    }
}

/// The inline script of `python -c CODE` / `perl -e CODE`.
fn inline_code(words: &[String], flags: &[&str]) -> Option<String> {
    words
        .windows(2)
        .find(|w| flags.contains(&w[0].as_str()))
        .map(|w| w[1].clone())
}

// ─── interpreter write sinks ────────────────────────────────────────

/// Obvious file-write idioms in python/perl code. String literals
/// only; anything computed is invisible (documented limit).
fn code_write_sinks(code: &str, out: &mut Vec<String>) {
    out.extend(open_sinks(code));
    out.extend(method_sinks(code, ".write_text("));
    out.extend(method_sinks(code, ".write_bytes("));
}

/// `open(...)` / `open FH, ...` calls that would create or truncate a
/// file. Python: `open("path", "w")`. Perl three-arg: `open(F, ">",
/// "path")` or `open F, ">", "path"`. Perl two-arg: `open(F, ">path")`.
fn open_sinks(code: &str) -> Vec<String> {
    let mut out = Vec::new();
    let chars: Vec<char> = code.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if !starts_word(&chars, i, "open") {
            i += 1;
            continue;
        }
        let mut j = i + 4;
        while j < chars.len() && chars[j].is_whitespace() {
            j += 1;
        }
        let paren = chars.get(j) == Some(&'(');
        if paren {
            j += 1;
        }
        let mut depth = usize::from(paren);
        let mut arg = String::new();
        let mut args: Vec<String> = Vec::new();
        while j < chars.len() {
            let c = chars[j];
            if paren {
                if c == '(' {
                    depth += 1;
                } else if c == ')' {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
            } else if c == ';' || c == '\n' {
                break;
            }
            if depth <= 1 && c == ',' {
                args.push(std::mem::take(&mut arg));
                j += 1;
                continue;
            }
            arg.push(c);
            j += 1;
        }
        args.push(arg);
        if let Some(path) = open_path(&args) {
            out.push(path);
        }
        i = j.max(i + 4);
    }
    out
}

/// `open` as a standalone word at `i` (not `reopen`, not `opened`).
fn starts_word(chars: &[char], i: usize, word: &str) -> bool {
    let w: Vec<char> = word.chars().collect();
    if i + w.len() > chars.len() {
        return false;
    }
    if i > 0 && is_ident_char(chars[i - 1]) {
        return false;
    }
    for (k, c) in w.iter().enumerate() {
        if chars[i + k] != *c {
            return false;
        }
    }
    !chars.get(i + w.len()).copied().is_some_and(is_ident_char)
}

/// Decide the path of an `open(...)` call. Returns `Some(path)` only
/// when a write mode is visible.
fn open_path(args: &[String]) -> Option<String> {
    let lits: Vec<String> = args
        .iter()
        .filter_map(|a| first_string_literal(a))
        .collect();
    if lits.is_empty() {
        return None;
    }
    let fh_is_bare = first_string_literal(args.first()?).is_none();
    if fh_is_bare {
        // perl: open(FH, MODE, PATH) or open(FH, ">PATH")
        if lits.len() >= 2 && is_write_mode_literal(&lits[0]) {
            return Some(lits[1].clone());
        }
        if lits.len() == 1 && (lits[0].starts_with('>') || lits[0].starts_with('+')) {
            let stripped = lits[0].trim_start_matches(['>', '+', '-']);
            return (!stripped.is_empty()).then(|| stripped.to_string());
        }
        return None;
    }
    // python: open(PATH, MODE, ...)
    let mode = lits.get(1)?;
    if is_write_mode_literal(mode) {
        return Some(lits[0].clone());
    }
    None
}

fn is_write_mode_literal(lit: &str) -> bool {
    if lit.is_empty() || lit.len() > 4 {
        return false;
    }
    // perl three-arg modes (">", ">>", "+>", …)
    if lit.starts_with('>') || lit.starts_with('+') {
        return true;
    }
    // python modes ("w", "a", "x", "w+", "wb", …) — never "r"
    let mut chars = lit.chars();
    let first = chars.next().unwrap_or(' ');
    matches!(first, 'w' | 'a' | 'x')
        && chars.all(|c| matches!(c, '+' | 'b' | 't' | 'U' | 'x' | 'N'))
}

/// `Path("p").write_text(` sinks: the receiver is a string literal or
/// `Path("literal")` immediately before the method.
fn method_sinks(code: &str, method: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut search_from = 0;
    while let Some(pos) = code[search_from..].find(method) {
        let at = search_from + pos;
        if let Some(path) = receiver_literal(&code[..at]) {
            out.push(path);
        }
        search_from = at + method.len();
    }
    out
}

/// Walk back from a method call to the receiver literal: `…Path("p")`
/// or `…"p"` / `…'p'`.
fn receiver_literal(before: &str) -> Option<String> {
    let t = before.trim_end();
    if let Some(rest) = t.strip_suffix(')') {
        let open = rest.rfind('(')?;
        let inner = &rest[open + 1..];
        let fname = rest[..open].trim_end();
        if fname.ends_with("Path") {
            return first_string_literal(inner);
        }
    }
    let chars: Vec<char> = t.chars().collect();
    let i = chars.len();
    if i == 0 {
        return None;
    }
    let quote = match chars[i - 1] {
        q @ ('"' | '\'') => q,
        _ => return None,
    };
    let mut j = i - 1;
    while j > 0 && chars[j - 1] != quote {
        j -= 1;
    }
    if j > 0 {
        let prev = chars[j - 1];
        if prev.is_alphanumeric() || prev == '_' {
            return None;
        }
    }
    let literal: String = chars[j..i - 1].iter().collect();
    (!literal.is_empty()).then_some(literal)
}

/// First `"…"` / `'…'` string literal inside a fragment.
fn first_string_literal(fragment: &str) -> Option<String> {
    let chars: Vec<char> = fragment.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '"' || chars[i] == '\'' {
            let q = chars[i];
            let mut j = i + 1;
            let mut lit = String::new();
            while j < chars.len() && chars[j] != q {
                if chars[j] == '\\' && j + 1 < chars.len() {
                    lit.push(chars[j + 1]);
                    j += 2;
                    continue;
                }
                lit.push(chars[j]);
                j += 1;
            }
            return Some(lit);
        }
        i += 1;
    }
    None
}

fn is_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

// ─── tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn targets(cmd: &str) -> Vec<String> {
        mutation_targets(cmd)
    }

    #[test]
    fn redirects_are_targets() {
        assert_eq!(targets("cat > out.txt <<'EOF'\nhi\nEOF"), ["out.txt"]);
        assert_eq!(targets("echo hi >> log.txt"), ["log.txt"]);
        assert_eq!(targets("printf x 2> err.log"), ["err.log"]);
        assert_eq!(
            targets("echo a > b.txt && echo c >> d.txt"),
            ["b.txt", "d.txt"]
        );
        assert_eq!(targets("python3 -c 'print(1)' > /dev/null"), ["/dev/null"]);
    }

    #[test]
    fn heredoc_payload_is_not_a_command() {
        // `>` inside the heredoc body must not be read as a redirect
        let cmd = "cat > f.txt <<'EOF'\nthis > that\nls > sneaky.txt\nEOF\n";
        assert_eq!(targets(cmd), ["f.txt"]);
    }

    #[test]
    fn sed_in_place_targets_operands() {
        assert_eq!(
            targets("sed -i 's/a/b/' shared/config.yaml"),
            ["shared/config.yaml"]
        );
        assert_eq!(targets("sed -i.bak 's/a/b/' f.txt"), ["f.txt"]);
        assert_eq!(
            targets("sed --in-place 's/a/b/' a.txt b.txt"),
            ["a.txt", "b.txt"]
        );
        assert_eq!(targets("sed -e 's/a/b/' -i g.txt"), ["g.txt"]);
    }

    #[test]
    fn sed_without_in_place_is_read_only() {
        assert_eq!(targets("sed -n '1p' f.txt"), Vec::<String>::new());
        assert_eq!(targets("sed 's/a/b/' f.txt"), Vec::<String>::new());
    }

    #[test]
    fn tee_targets_outputs() {
        assert_eq!(targets("echo hi | tee out.txt"), ["out.txt"]);
        assert_eq!(targets("tee -a a.log b.log"), ["a.log", "b.log"]);
    }

    #[test]
    fn cp_mv_target_is_the_destination() {
        assert_eq!(targets("cp src.txt dst.txt"), ["dst.txt"]);
        assert_eq!(targets("mv old.txt new.txt"), ["new.txt"]);
        assert_eq!(targets("cp -r a b c dir/"), ["dir/"]);
        assert_eq!(targets("cp -t dest/ a b"), ["dest/"]);
    }

    #[test]
    fn python_c_write_sinks() {
        assert_eq!(
            targets(r#"python3 -c "open('out.txt','w').write('x')""#),
            ["out.txt"]
        );
        assert_eq!(
            targets(r#"python3 -c "open('a.txt','w').write('1'); open('b.txt','a').write('2')""#),
            ["a.txt", "b.txt"]
        );
        assert_eq!(
            targets(r#"python3 -c "from pathlib import Path; Path('out.txt').write_text('hi')""#),
            ["out.txt"]
        );
    }

    #[test]
    fn perl_e_write_sinks() {
        assert_eq!(
            targets(r#"perl -e 'open(F, ">", "out.txt") or die; print F "x"'"#),
            ["out.txt"]
        );
        assert_eq!(
            targets(r#"perl -e 'open F, ">>", "log.txt" or die'"#),
            ["log.txt"]
        );
        assert_eq!(
            targets(r#"perl -e 'open(F, ">out.txt") or die'"#),
            ["out.txt"]
        );
    }

    #[test]
    fn nested_shell_scripts_are_parsed() {
        assert_eq!(targets("sh -c 'cat > f.txt <<EOF\nhi\nEOF'"), ["f.txt"]);
        assert_eq!(targets(r#"bash -c "echo x > inner.log""#), ["inner.log"]);
    }

    #[test]
    fn heredoc_interpreter_scripts_are_parsed() {
        let cmd = "python3 - <<'PY'\nopen('from-heredoc.txt','w').write('x')\nPY\n";
        assert_eq!(targets(cmd), ["from-heredoc.txt"]);
    }

    #[test]
    fn read_only_commands_are_not_mutations() {
        assert_eq!(targets("cat f.txt"), Vec::<String>::new());
        assert_eq!(targets("cat f.txt g.txt"), Vec::<String>::new());
        assert_eq!(targets("grep pattern f.txt"), Vec::<String>::new());
        assert_eq!(targets("ls -la shared/"), Vec::<String>::new());
        assert_eq!(targets("echo hi"), Vec::<String>::new());
        assert_eq!(targets("head -5 f.txt"), Vec::<String>::new());
        assert_eq!(
            targets(r#"python3 -c "print(open('f.txt').read())""#),
            Vec::<String>::new()
        );
        assert_eq!(targets("diff a.txt b.txt"), Vec::<String>::new());
        assert_eq!(targets("sed -n p f.txt | grep x"), Vec::<String>::new());
    }

    #[test]
    fn quoted_redirects_and_dupes() {
        // fd duplication is not a file write
        assert_eq!(targets("cmd 2>&1"), Vec::<String>::new());
        assert_eq!(targets("cmd > out.txt 2>&1"), ["out.txt"]);
        // here-string is not a file write
        assert_eq!(targets("cat <<< 'hi'"), Vec::<String>::new());
    }

    #[test]
    fn workspace_targets_filter_and_resolve() {
        let cwd = Path::new("/ws");
        let root = Path::new("/ws");
        assert_eq!(
            workspace_targets("cat > shared/config.yaml <<EOF\nx\nEOF", cwd, root),
            [PathBuf::from("/ws/shared/config.yaml")]
        );
        assert_eq!(
            workspace_targets("sed -i 's/a/b/' /etc/hosts", cwd, root),
            Vec::<PathBuf>::new()
        );
        assert_eq!(
            workspace_targets("cat > ../escape.txt", cwd, root),
            Vec::<PathBuf>::new()
        );
    }
}
