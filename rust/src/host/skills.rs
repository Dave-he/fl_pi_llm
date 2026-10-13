//! Skills for a Rust host: folders holding a `SKILL.md` (front matter with a
//! `name` and a `description`, then instructions) and whatever files it
//! points to, in one directory the host keeps.
//!
//! What the model is told and the tool it loads one with are the Dart side's
//! (`ui/lib/src/skills/skills.dart`, `tools/func/skill.dart`): the system
//! prompt lists each skill's directory and description, and `skill` reads its
//! SKILL.md, the list of its files, or one of them.

use std::path::{Component, Path, PathBuf};

use serde_json::json;
use yaml_rust2::{Yaml, YamlLoader};

use super::{BoxFuture, Cancel, Tool, ToolCall, ToolDef, ToolResult};

/// The tool's name, as the Dart side calls it.
pub const TOOL_NAME: &str = "skill";
/// What one read of a file gives the model, at most.
pub const MAX_READ_CHARS: usize = 256 * 1024;
/// What the prompt carries of a description.
const MAX_DESCRIPTION: usize = 1024;
/// Files listed of one skill, at most.
const MAX_FILES: usize = 200;

/// A skill found in the directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    /// Its directory's name: what the model asks for it by.
    pub dir: String,
    pub name: String,
    pub description: String,
}

/// A SKILL.md's front matter and the instructions after it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillMd {
    pub name: String,
    pub description: String,
    pub body: String,
}

/// One line of text: no control characters, no escape sequences.
pub fn clean(s: &str) -> String {
    static ESC: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"\x1B\[[0-9;?]*[ -/]*[@-~]").expect("regex"));
    let s = ESC.replace_all(s, "");
    let mut out = String::with_capacity(s.len());
    let mut space = false;
    for c in s.chars() {
        if matches!(c, '\r' | '\n' | '\t') {
            space = true;
            continue;
        }
        if c.is_control() {
            continue;
        }
        if space && !out.is_empty() {
            out.push(' ');
        }
        space = false;
        out.push(c);
    }
    out.trim().to_string()
}

/// [md]'s front matter: None unless it names the skill and says what it is
/// for, both as text.
pub fn parse(md: &str) -> Option<SkillMd> {
    let md = md.strip_prefix('\u{feff}').unwrap_or(md).replace("\r\n", "\n");
    let rest = md.strip_prefix("---\n")?;
    let (front, body) = match rest.find("\n---") {
        Some(at) => {
            let after = &rest[at + 4..];
            // The closing line is `---` alone.
            if !(after.is_empty() || after.starts_with('\n')) {
                return None;
            }
            (&rest[..at], after.strip_prefix('\n').unwrap_or(after))
        }
        None => return None,
    };
    let docs = YamlLoader::load_from_str(front).ok()?;
    let doc = docs.first()?;
    let (Yaml::String(name), Yaml::String(description)) = (&doc["name"], &doc["description"]) else { return None };
    let (name, description) = (clean(name), clean(description));
    if name.is_empty() || description.is_empty() {
        return None;
    }
    Some(SkillMd { name, description, body: body.to_string() })
}

/// [rel] resolved under [base], or None when it would leave it.
fn inside(base: &Path, rel: &str) -> Option<PathBuf> {
    let rel = Path::new(rel);
    if rel.is_absolute() {
        return None;
    }
    let mut out = base.to_path_buf();
    for c in rel.components() {
        match c {
            Component::Normal(p) => out.push(p),
            Component::CurDir => {}
            _ => return None,
        }
    }
    (out != base).then_some(out)
}

/// The skills in one directory: `<root>/<dir>/SKILL.md`.
#[derive(Debug, Clone)]
pub struct Skills {
    root: PathBuf,
}

impl Skills {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Every skill there, by directory name; a folder without a readable
    /// SKILL.md is not one.
    pub fn list(&self) -> Vec<Skill> {
        let Ok(entries) = std::fs::read_dir(&self.root) else { return Vec::new() };
        let mut out: Vec<Skill> = entries
            .flatten()
            .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
            .filter_map(|e| {
                let dir = e.file_name().to_string_lossy().into_owned();
                if dir.starts_with('.') {
                    return None;
                }
                let md = parse(&std::fs::read_to_string(e.path().join("SKILL.md")).ok()?)?;
                Some(Skill { dir, name: md.name, description: md.description })
            })
            .collect();
        out.sort_by(|a, b| a.dir.cmp(&b.dir));
        out
    }

    /// [file] of skill [dir] as text; None when there is no such text file
    /// in it.
    pub fn read(&self, dir: &str, file: &str) -> Option<String> {
        let base = inside(&self.root, dir)?;
        let path = inside(&base, file)?;
        // A link may point anywhere: only what is really in the folder.
        let real = std::fs::canonicalize(&path).ok()?;
        if !real.starts_with(std::fs::canonicalize(&base).ok()?) {
            return None;
        }
        String::from_utf8(std::fs::read(real).ok()?).ok()
    }

    /// The files of skill [dir], SKILL.md first, by path relative to it.
    pub fn files_of(&self, dir: &str) -> Vec<String> {
        let Some(base) = inside(&self.root, dir) else { return Vec::new() };
        let mut files = Vec::new();
        let mut stack = vec![base.clone()];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
                let Ok(t) = e.file_type() else { continue };
                if t.is_dir() {
                    stack.push(e.path());
                } else if t.is_file()
                    && let Ok(rel) = e.path().strip_prefix(&base)
                {
                    files.push(rel.to_string_lossy().replace('\\', "/"));
                }
            }
        }
        files.sort();
        if let Some(at) = files.iter().position(|f| f == "SKILL.md") {
            let md = files.remove(at);
            files.insert(0, md);
        }
        files
    }
}

/// The skills part of the system prompt for [skills]: what each is for, the
/// model to load one with the `skill` tool when a task matches it. None
/// without any.
pub fn prompt(skills: &[Skill]) -> Option<String> {
    if skills.is_empty() {
        return None;
    }
    let list: Vec<String> = skills
        .iter()
        .map(|s| {
            let d = match s.description.char_indices().nth(MAX_DESCRIPTION) {
                Some((at, _)) => format!("{}…", &s.description[..at]),
                None => s.description.clone(),
            };
            format!("- {}: {d}", s.dir)
        })
        .collect();
    Some(format!(
        "# Skills\n\nSkills are instructions for particular tasks that the user installed. When a task matches a skill's \
description, load the skill with the `{TOOL_NAME}` tool before starting the task, and follow it. A skill may point to its \
other files by relative path; read those with the same tool when you need them. Scripts in a skill cannot run here unless \
another tool can run them.\n\nAvailable skills:\n{}",
        list.join("\n")
    ))
}

/// A skill's directory name, as the `skills` CLI makes it (`sanitizeName`).
pub fn dir_name(name: &str) -> String {
    static NOT: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| regex::Regex::new(r"[^a-z0-9._]+").expect("regex"));
    let s = NOT.replace_all(&name.to_lowercase(), "-").into_owned();
    let s = s.trim_matches(|c| c == '.' || c == '-');
    let s: String = s.chars().take(255).collect();
    if s.is_empty() { "unnamed-skill".into() } else { s }
}

/// A skill found among a source's files.
#[derive(Debug, Clone)]
pub struct FoundSkill {
    pub name: String,
    pub description: String,
    /// Its folder in the source; empty for the source's root.
    pub dir: String,
    /// Its files by path relative to [dir], `SKILL.md` by that name.
    pub files: std::collections::BTreeMap<String, Vec<u8>>,
}

/// Where agents keep skills in a repository, looked in below the root.
const CONTAINERS: &[&str] = &[
    "skills", "skills/.curated", "skills/.experimental", "skills/.system", ".agents/skills", ".claude/skills", ".cline/skills",
    ".codebuddy/skills", ".codex/skills", ".commandcode/skills", ".continue/skills", ".factory/skills", ".github/skills",
    ".goose/skills", ".grok/skills", ".iflow/skills", ".junie/skills", ".kilo/skills", ".kilocode/skills", ".kimchi/skills",
    ".kiro/skills", ".minimax/skills", ".mux/skills", ".neovate/skills", ".opencode/skills", ".openhands/skills", ".pi/skills",
    ".posit/assistant/skills", ".qoder/skills", ".roo/skills", ".trae/skills", ".windsurf/skills", ".zcode/skills",
    ".zencoder/skills",
];
const CONTAINER_DEPTH: usize = 3;
const FALLBACK_DEPTH: usize = 5;
const SKIP_DIRS: &[&str] = &["node_modules", ".git", "dist", "build", "__pycache__"];
const SKIP_FILES: &[&str] = &["metadata.json"];
const SKIP_FILE_DIRS: &[&str] = &[".git", "__pycache__", "__pypackages__"];

/// The skills among a source's [files] (by path), under [path] when given:
/// one there, else those below it, the `skills` CLI's way
/// (`ui/lib/src/skills/discover.dart`, `SkillDiscovery.find`).
pub fn find(files: &std::collections::BTreeMap<String, Vec<u8>>, path: Option<&str>) -> Vec<FoundSkill> {
    use std::collections::BTreeSet;
    let base = path.filter(|p| !p.is_empty()).map(|p| format!("{p}/")).unwrap_or_default();
    let mut skill_dirs = BTreeSet::new();
    for p in files.keys().filter(|p| p.starts_with(&base)) {
        let (dir, name) = p.rsplit_once('/').map_or(("", p.as_str()), |(d, n)| (d, n));
        if name.eq_ignore_ascii_case("skill.md") {
            skill_dirs.insert(dir.to_string());
        }
    }
    let root = base.strip_suffix('/').unwrap_or("").to_string();
    let mut found: Vec<FoundSkill> = Vec::new();
    let mut seen = BTreeSet::new();
    let mut add = |dir: &str, found: &mut Vec<FoundSkill>| {
        if let Some(s) = load(files, dir)
            && seen.insert(s.name.clone())
        {
            found.push(s);
        }
    };
    if skill_dirs.contains(&root) {
        add(&root, &mut found);
        if !found.is_empty() {
            return found;
        }
    }
    // Folders below [dir] with a SKILL.md, not below one that has it.
    let walk = |dir: &str, depth: usize| -> Vec<String> {
        let prefix = if dir.is_empty() { String::new() } else { format!("{dir}/") };
        let mut out: Vec<String> = skill_dirs
            .iter()
            .filter(|d| d.as_str() != dir && d.starts_with(&prefix))
            .filter(|d| {
                let rel: Vec<&str> = d[prefix.len()..].split('/').collect();
                rel.len() <= depth
                    && !rel.iter().any(|p| SKIP_DIRS.contains(p))
                    && !(1..rel.len()).any(|i| skill_dirs.contains(&format!("{prefix}{}", rel[..i].join("/"))))
            })
            .cloned()
            .collect();
        out.sort();
        out
    };
    for d in walk(&root, 1) {
        add(&d, &mut found);
    }
    for c in CONTAINERS {
        let at = if root.is_empty() { c.to_string() } else { format!("{root}/{c}") };
        for d in walk(&at, CONTAINER_DEPTH) {
            add(&d, &mut found);
        }
    }
    if found.is_empty() {
        for d in walk(&root, FALLBACK_DEPTH) {
            add(&d, &mut found);
        }
    }
    found
}

fn load(files: &std::collections::BTreeMap<String, Vec<u8>>, dir: &str) -> Option<FoundSkill> {
    let prefix = if dir.is_empty() { String::new() } else { format!("{dir}/") };
    let (md_key, md) = files.iter().find(|(k, _)| k.starts_with(&prefix) && k[prefix.len()..].eq_ignore_ascii_case("skill.md"))?;
    let parsed = parse(&String::from_utf8_lossy(md))?;
    let keeps = |rel: &str| {
        let parts: Vec<&str> = rel.split('/').collect();
        !SKIP_FILES.contains(parts.last().unwrap_or(&"")) && !parts[..parts.len() - 1].iter().any(|p| SKIP_FILE_DIRS.contains(p))
    };
    let files = files
        .iter()
        .filter(|(k, _)| k.starts_with(&prefix) && keeps(&k[prefix.len()..]))
        .map(|(k, v)| (if k == md_key { "SKILL.md".to_string() } else { k[prefix.len()..].to_string() }, v.clone()))
        .collect();
    Some(FoundSkill { name: parsed.name, description: parsed.description, dir: dir.to_string(), files })
}

/// A GitHub repository skills are installed from, as `npx skills add` takes
/// it: `owner/repo`, `owner/repo/path`, `owner/repo@skill`, `github:…`,
/// `https://github.com/owner/repo[/tree/<ref>/<path>]`, with `#ref[@skill]`,
/// or a whole `npx skills add <source> [--skill <name>]` line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitHubSource {
    pub owner: String,
    pub repo: String,
    pub r#ref: Option<String>,
    pub path: Option<String>,
    /// The one skill wanted of several; None for all of them.
    pub skill: Option<String>,
}

impl GitHubSource {
    // TODO: GitLab, site (`/.well-known`) and download sources, which the
    // Dart side (`ui/lib/src/skills/source.dart`) reads too, once the app
    // installs through this.
    pub fn parse(input: &str) -> Result<Self, String> {
        let words: Vec<&str> = input.split_whitespace().collect();
        let words = match words.iter().position(|w| *w == "add") {
            Some(add) if add > 0 && words[add - 1] == "skills" => &words[add + 1..],
            _ => &words[..],
        };
        let (mut skill, mut source) = (None, None);
        let mut i = 0;
        while i < words.len() {
            let w = words[i];
            if (w == "--skill" || w == "-s") && i + 1 < words.len() {
                i += 1;
                skill = Some(words[i].to_string());
            } else if let Some(s) = w.strip_prefix("--skill=") {
                skill = Some(s.to_string());
            } else if !w.starts_with('-') && source.is_none() {
                source = Some(w);
            }
            i += 1;
        }
        let mut parsed = Self::parse_one(source.ok_or("no source")?)?;
        if let Some(s) = skill.filter(|s| s != "*") {
            parsed.skill = Some(s);
        }
        Ok(parsed)
    }

    fn parse_one(input: &str) -> Result<Self, String> {
        let (mut s, mut r#ref, mut frag_skill) = (input.to_string(), None, None);
        if let Some(hash) = s.find('#').filter(|h| *h > 0) {
            let frag = s[hash + 1..].to_string();
            s.truncate(hash);
            let (r, k) = frag.split_once('@').map_or((frag.as_str(), None), |(r, k)| (r, Some(k.to_string())));
            r#ref = Some(r.to_string()).filter(|r| !r.is_empty());
            frag_skill = k;
        }
        let s = s.strip_prefix("github:").unwrap_or(&s);
        let repo = |r: &str| r.strip_suffix(".git").unwrap_or(r).to_string();
        let subpath = |p: &str| -> Result<Option<String>, String> {
            let parts: Vec<&str> = p.split(['/', '\\']).filter(|e| !e.is_empty() && *e != ".").collect();
            if parts.contains(&"..") {
                return Err("a path out of the source".into());
            }
            Ok((!parts.is_empty()).then(|| parts.join("/")))
        };
        if let Some(rest) = s.strip_prefix("https://").or_else(|| s.strip_prefix("http://")) {
            let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
            if !matches!(host.to_ascii_lowercase().as_str(), "github.com" | "www.github.com") {
                return Err("only GitHub repositories".into());
            }
            let segs: Vec<&str> = path.split(['?', '#']).next().unwrap_or("").split('/').filter(|e| !e.is_empty()).collect();
            if segs.len() < 2 {
                return Err("not a repository".into());
            }
            if segs.len() > 2 && matches!(segs[2], "archive" | "raw" | "releases") {
                return Err("only GitHub repositories".into());
            }
            let mut out = Self { owner: segs[0].into(), repo: repo(segs[1]), r#ref, path: None, skill: frag_skill };
            if segs.len() >= 4 && (segs[2] == "tree" || segs[2] == "blob") {
                let mut rest = segs[4..].to_vec();
                if segs[2] == "blob" && rest.last().is_some_and(|l| l.eq_ignore_ascii_case("skill.md")) {
                    rest.pop();
                }
                out.r#ref = Some(segs[3].to_string());
                out.path = subpath(&rest.join("/"))?;
            }
            return Ok(out);
        }
        static AT_SKILL: std::sync::LazyLock<regex::Regex> =
            std::sync::LazyLock::new(|| regex::Regex::new(r"^([^/:.][^/:]*)/([^/@:]+)@(.+)$").expect("regex"));
        static SHORT: std::sync::LazyLock<regex::Regex> =
            std::sync::LazyLock::new(|| regex::Regex::new(r"^([^/:.][^/:]*)/([^/:]+?)(?:/(.+?))?/?$").expect("regex"));
        if let Some(c) = AT_SKILL.captures(s) {
            return Ok(Self { owner: c[1].into(), repo: repo(&c[2]), r#ref, path: None, skill: frag_skill.or(Some(c[3].into())) });
        }
        if let Some(c) = SHORT.captures(s) {
            let path = match c.get(3) {
                Some(p) => subpath(p.as_str())?,
                None => None,
            };
            return Ok(Self { owner: c[1].into(), repo: repo(&c[2]), r#ref, path, skill: frag_skill });
        }
        Err("not a skill source".into())
    }

    /// Where its archive is: GitHub's own download host, which answers
    /// without a redirect.
    pub fn archive_url(&self) -> String {
        format!("https://codeload.github.com/{}/{}/tar.gz/{}", self.owner, self.repo, self.r#ref.as_deref().unwrap_or("HEAD"))
    }

    /// `owner/repo[/path]`, as a person reads it.
    pub fn id(&self) -> String {
        let mut id = format!("{}/{}", self.owner, self.repo);
        if let Some(p) = &self.path {
            id = format!("{id}/{p}");
        }
        id
    }
}

/// The `skill` tool over [skills], offering only [offered] (directory names).
pub struct SkillTool {
    pub skills: Skills,
    pub offered: Vec<Skill>,
}

impl Tool for SkillTool {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: TOOL_NAME.into(),
            description: "Load an installed skill: its instructions (SKILL.md) and the list of its files, or one of those files."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "The skill, as the list of available skills names it." },
                    "file": {
                        "type": "string",
                        "description": "A file of the skill, by its path relative to the skill. Default: SKILL.md, with the list of its files.",
                    },
                },
                "required": ["name"],
            }),
            label: Some("Skill".into()),
            sequential: false,
        }
    }

    fn execute(&self, call: ToolCall, _cancel: Cancel) -> BoxFuture<'_, Result<ToolResult, String>> {
        Box::pin(async move {
            let name = call.args["name"].as_str().ok_or("name is required")?;
            let skill = self.offered.iter().find(|s| s.dir == name).or_else(|| self.offered.iter().find(|s| s.name == name));
            let Some(skill) = skill else {
                let names: Vec<&str> = self.offered.iter().map(|s| s.dir.as_str()).collect();
                return Err(format!("No skill \"{name}\". Available: {}", names.join(", ")));
            };
            let file = call.args["file"].as_str().filter(|f| !f.is_empty());
            let text = self
                .skills
                .read(&skill.dir, file.unwrap_or("SKILL.md"))
                .ok_or_else(|| format!("No text file \"{}\" in {}", file.unwrap_or("SKILL.md"), skill.dir))?;
            let body = match text.char_indices().nth(MAX_READ_CHARS) {
                Some((at, _)) => format!("{}\n… (truncated)", &text[..at]),
                None => text,
            };
            if file.is_some() {
                return Ok(ToolResult::text(body));
            }
            let others: Vec<String> =
                self.skills.files_of(&skill.dir).into_iter().filter(|f| f != "SKILL.md").take(MAX_FILES).map(|f| format!("- {f}")).collect();
            Ok(ToolResult::text(if others.is_empty() {
                body
            } else {
                format!("{body}\n\nFiles of this skill (read one with `file`):\n{}", others.join("\n"))
            }))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn front_matter() {
        let md = "\u{feff}---\r\nname: Docker cleanup\r\ndescription: >\r\n  Frees disk space\r\n  safely.\r\n---\r\n# Steps\n";
        assert_eq!(
            parse(md),
            Some(SkillMd { name: "Docker cleanup".into(), description: "Frees disk space safely.".into(), body: "# Steps\n".into() })
        );
        assert_eq!(parse("---\nname: a\n---\n"), None, "no description");
        assert_eq!(parse("---\nname: a\ndescription: [1]\n---\n"), None, "not text");
        assert_eq!(parse("# no front matter"), None);
        assert_eq!(clean("a\u{1b}[31m red\u{1b}[0m\n\tb\u{7}"), "a red b");
    }

    #[test]
    fn sources_read_as_the_cli_reads_them() {
        let gh = |owner: &str, repo: &str, r: Option<&str>, path: Option<&str>, skill: Option<&str>| GitHubSource {
            owner: owner.into(),
            repo: repo.into(),
            r#ref: r.map(String::from),
            path: path.map(String::from),
            skill: skill.map(String::from),
        };
        assert_eq!(GitHubSource::parse("anthropics/skills").unwrap(), gh("anthropics", "skills", None, None, None));
        assert_eq!(GitHubSource::parse("npx skills add owner/repo.git/skills/pdf -y").unwrap(), gh("owner", "repo", None, Some("skills/pdf"), None));
        assert_eq!(GitHubSource::parse("owner/repo@pdf").unwrap(), gh("owner", "repo", None, None, Some("pdf")));
        assert_eq!(GitHubSource::parse("github:owner/repo#v2@pdf").unwrap(), gh("owner", "repo", Some("v2"), None, Some("pdf")));
        assert_eq!(
            GitHubSource::parse("https://github.com/owner/repo/blob/main/skills/pdf/SKILL.md").unwrap(),
            gh("owner", "repo", Some("main"), Some("skills/pdf"), None)
        );
        assert_eq!(GitHubSource::parse("bunx skills add owner/repo --skill pdf").unwrap().skill.as_deref(), Some("pdf"));
        assert!(GitHubSource::parse("owner/repo/../x").is_err());
        assert!(GitHubSource::parse("https://gitlab.com/a/b").is_err());
        assert_eq!(gh("o", "r", None, None, None).archive_url(), "https://codeload.github.com/o/r/tar.gz/HEAD");
    }

    #[test]
    fn skills_are_found_where_the_cli_finds_them() {
        let md = |name: &str| format!("---\nname: {name}\ndescription: Does {name}.\n---\n").into_bytes();
        let files: std::collections::BTreeMap<String, Vec<u8>> = [
            ("README.md", b"x".to_vec()),
            ("skills/pdf/skill.md", md("PDF Tools")),
            ("skills/pdf/scripts/a.py", b"print()".to_vec()),
            ("skills/pdf/__pycache__/a.pyc", b"x".to_vec()),
            ("skills/pdf/metadata.json", b"{}".to_vec()),
            ("skills/pdf/inner/SKILL.md", md("Inner")),
            (".claude/skills/web/SKILL.md", md("Web")),
            ("node_modules/x/SKILL.md", md("No")),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
        let found = find(&files, None);
        let names: Vec<&str> = found.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["PDF Tools", "Web"], "the nearest SKILL.md takes in the ones below it");
        assert_eq!(found[0].files.keys().collect::<Vec<_>>(), ["SKILL.md", "inner/SKILL.md", "scripts/a.py"]);
        assert_eq!(find(&files, Some("skills/pdf/inner"))[0].name, "Inner");
        assert_eq!(dir_name("PDF Tools!"), "pdf-tools");
        assert_eq!(dir_name("..."), "unnamed-skill");
    }

    #[tokio::test]
    async fn the_tool_reads_what_is_in_a_skill_only() {
        let root = std::env::temp_dir().join(format!("fl-pi-llm-skills-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let dir = root.join("cleanup");
        std::fs::create_dir_all(dir.join("scripts")).unwrap();
        std::fs::write(dir.join("SKILL.md"), "---\nname: Cleanup\ndescription: Frees space.\n---\nSee scripts/run.sh\n").unwrap();
        std::fs::write(dir.join("scripts/run.sh"), "docker system prune\n").unwrap();
        std::fs::create_dir_all(root.join("broken")).unwrap();
        std::fs::write(root.join("secret.txt"), "no").unwrap();

        let skills = Skills::new(&root);
        let list = skills.list();
        assert_eq!(list, vec![Skill { dir: "cleanup".into(), name: "Cleanup".into(), description: "Frees space.".into() }]);
        assert!(prompt(&list).unwrap().ends_with("Available skills:\n- cleanup: Frees space."));
        assert_eq!(prompt(&[]), None);
        assert_eq!(skills.read("cleanup", "../secret.txt"), None);
        assert_eq!(skills.read("..", "secret.txt"), None);

        let tool = SkillTool { skills: skills.clone(), offered: list };
        let call = |args| ToolCall { session_id: "s".into(), id: "c".into(), name: TOOL_NAME.into(), args };
        let res = tool.execute(call(json!({ "name": "Cleanup" })), Cancel::new()).await.unwrap();
        assert!(res.content[0]["text"].as_str().unwrap().ends_with("(read one with `file`):\n- scripts/run.sh"));
        let res = tool.execute(call(json!({ "name": "cleanup", "file": "scripts/run.sh" })), Cancel::new()).await.unwrap();
        assert_eq!(res.content[0]["text"], "docker system prune\n");
        assert!(tool.execute(call(json!({ "name": "broken" })), Cancel::new()).await.is_err());
        assert!(tool.execute(call(json!({ "name": "cleanup", "file": "../../secret.txt" })), Cancel::new()).await.is_err());
        std::fs::remove_dir_all(&root).unwrap();
    }
}
