//! Dockerfile parsing: logical lines, stages and instruction words.

use anyhow::{bail, Context, Result};

#[derive(Debug, Clone)]
pub struct Instr {
    pub cmd: String,
    pub rest: String,
}

#[derive(Debug)]
pub struct Stage {
    pub name: Option<String>,
    pub from: String,
    /// `FROM --platform=...`, unexpanded
    pub platform: Option<String>,
    pub instrs: Vec<Instr>,
}

fn logical_lines(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.starts_with('#') || (line.is_empty() && cur.is_empty()) {
            continue;
        }
        match line.strip_suffix('\\') {
            Some(head) => {
                cur.push_str(head);
                cur.push(' ');
            }
            None => {
                cur.push_str(line);
                out.push(std::mem::take(&mut cur));
            }
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur);
    }
    out
}

/// The instructions before the first FROM (only ARG is allowed there), and the stages.
pub fn parse(text: &str) -> Result<(Vec<Instr>, Vec<Stage>)> {
    let mut globals = Vec::new();
    let mut stages: Vec<Stage> = Vec::new();
    for line in logical_lines(text) {
        let (cmd, rest) = line.split_once(char::is_whitespace).unwrap_or((&line, ""));
        let (cmd, rest) = (cmd.to_ascii_uppercase(), rest.trim().to_string());
        if cmd == "FROM" {
            let all: Vec<&str> = rest.split_whitespace().collect();
            let platform = all.iter().find_map(|w| w.strip_prefix("--platform=")).map(String::from);
            let words: Vec<&str> = all.into_iter().filter(|w| !w.starts_with("--")).collect();
            let from = words.first().context("FROM without an image")?.to_string();
            let name = (words.len() >= 3 && words[1].eq_ignore_ascii_case("as")).then(|| words[2].to_string());
            stages.push(Stage { name, from, platform, instrs: Vec::new() });
        } else if let Some(s) = stages.last_mut() {
            s.instrs.push(Instr { cmd, rest });
        } else if cmd == "ARG" {
            globals.push(Instr { cmd, rest });
        } else {
            bail!("{cmd} before the first FROM");
        }
    }
    if stages.is_empty() {
        bail!("no FROM in the Dockerfile");
    }
    Ok((globals, stages))
}

pub fn find_stage(stages: &[Stage], reference: &str) -> Option<usize> {
    stages.iter().position(|s| s.name.as_deref() == Some(reference)).or_else(|| reference.parse::<usize>().ok().filter(|&i| i < stages.len()))
}

pub fn target_index(stages: &[Stage], target: Option<&str>) -> Result<usize> {
    match target {
        None => Ok(stages.len() - 1),
        Some(t) => find_stage(stages, t).with_context(|| format!("no stage {t}")),
    }
}

pub fn copy_flags(rest: &str) -> (Vec<(String, String)>, Vec<String>) {
    let mut flags = Vec::new();
    let mut args = Vec::new();
    for w in words(rest) {
        match w.strip_prefix("--").and_then(|f| f.split_once('=')) {
            Some((k, v)) if args.is_empty() => flags.push((k.to_string(), v.to_string())),
            _ => args.push(w),
        }
    }
    (flags, args)
}

/// Splits on whitespace, honouring quotes.
pub fn words(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let (mut cur, mut quote, mut any) = (String::new(), None, false);
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match (c, quote) {
            ('\\', _) => {
                if let Some(n) = chars.next() {
                    cur.push(n);
                    any = true;
                }
            }
            ('"' | '\'', None) => {
                quote = Some(c);
                any = true;
            }
            (c, Some(q)) if c == q => quote = None,
            (c, None) if c.is_whitespace() => {
                if any {
                    out.push(std::mem::take(&mut cur));
                    any = false;
                }
            }
            (c, _) => {
                cur.push(c);
                any = true;
            }
        }
    }
    if any {
        out.push(cur);
    }
    out
}

pub fn clean_src(s: &str) -> &str {
    let s = s.strip_suffix("/.").unwrap_or(s);
    let s = s.strip_prefix("./").unwrap_or(s);
    if s == "." { s } else { s.trim_end_matches('/') }
}

pub fn join_abs(workdir: Option<&str>, p: &str) -> String {
    if p.starts_with('/') {
        p.to_string()
    } else {
        format!("{}/{}", workdir.unwrap_or("/").trim_end_matches('/'), p.strip_prefix("./").unwrap_or(p))
    }
}

pub fn json_list(rest: &str) -> Option<Vec<String>> {
    rest.trim_start().starts_with('[').then(|| serde_json::from_str(rest).ok()).flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_stages_globals_and_continuations() {
        let (g, st) = parse("ARG V=1\nFROM --platform=$BUILDPLATFORM a:$V AS b\nRUN x \\\n  # c\n  && y\nFROM b\nCOPY --chown=1:1 \"a b\" c/\n").unwrap();
        assert_eq!(g[0].rest, "V=1");
        assert_eq!(st[0].platform.as_deref(), Some("$BUILDPLATFORM"));
        assert_eq!((st[0].name.as_deref(), st[0].from.as_str()), (Some("b"), "a:$V"));
        assert_eq!(st[0].instrs[0].rest, "x  && y");
        let (flags, args) = copy_flags(&st[1].instrs[0].rest);
        assert_eq!(flags, vec![("chown".to_string(), "1:1".to_string())]);
        assert_eq!(args, vec!["a b", "c/"]);
    }
}
