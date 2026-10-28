//! `cargo xtask docs` builds the documentation site with mdBook.
//!
//! The book's pages are the repository's own Markdown files, listed by
//! repository path in `docs/book/SUMMARY.md`. The task copies each page to the
//! same path under `target/book/src`, so links between pages keep working. A
//! link to any other repository file becomes a link to that file on GitHub, and
//! a link to a path that does not exist fails the build.

use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, ensure};

use crate::root;

/// Base URL for links to repository files that are not book pages.
const GITHUB: &str = "https://github.com/smrt-devops/buildkit-controller";

/// Build the site into `target/book/html`.
pub(crate) fn docs() -> Result<()> {
    let root = root();
    let book = root.join("docs/book");
    let out = root.join("target/book");
    let src = out.join("src");
    if out.exists() {
        std::fs::remove_dir_all(&out).with_context(|| format!("removing {}", out.display()))?;
    }
    std::fs::create_dir_all(&src)?;
    let summary = std::fs::read_to_string(book.join("SUMMARY.md")).context("reading SUMMARY.md")?;
    std::fs::write(src.join("SUMMARY.md"), &summary)?;
    std::fs::copy(book.join("book.toml"), out.join("book.toml")).context("copying book.toml")?;

    let pages = summary_pages(&summary);
    let exists = |p: &Path| {
        let full = root.join(p);
        full.exists().then(|| full.is_dir())
    };
    let mut broken = Vec::new();
    for page in &pages {
        let text = std::fs::read_to_string(root.join(page))
            .with_context(|| format!("reading {}", page.display()))?;
        match rewrite(&text, page, &pages, exists) {
            Ok(text) => {
                let dest = src.join(page);
                if let Some(dir) = dest.parent() {
                    std::fs::create_dir_all(dir)?;
                }
                std::fs::write(&dest, text)?;
            }
            Err(mut e) => broken.append(&mut e),
        }
    }
    ensure!(broken.is_empty(), "broken links:\n{}", broken.join("\n"));

    let status = Command::new("mdbook")
        .arg("build")
        .arg(&out)
        .status()
        .context("running mdbook (install it with `cargo install mdbook --locked`)")?;
    ensure!(status.success(), "mdbook build failed with {status}");
    println!("built {}", out.join("html").display());
    Ok(())
}

/// The repository paths that `SUMMARY.md` links to.
fn summary_pages(summary: &str) -> BTreeSet<PathBuf> {
    summary
        .lines()
        .flat_map(link_targets)
        .map(|(_, t)| PathBuf::from(t))
        .collect()
}

/// Byte ranges and text of the inline link targets on one line.
fn link_targets(line: &str) -> Vec<(std::ops::Range<usize>, &str)> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(i) = line[from..].find("](") {
        let start = from + i + 2;
        let len = line[start..].find([')', ' ']).unwrap_or(line.len() - start);
        out.push((start..start + len, &line[start..start + len]));
        from = start + len;
    }
    out
}

/// Rewrite the links of `page` for the book. `exists` reports whether a
/// repository path is a directory, or `None` when it does not exist. Returns
/// every broken link on failure.
fn rewrite(
    text: &str,
    page: &Path,
    pages: &BTreeSet<PathBuf>,
    exists: impl Fn(&Path) -> Option<bool>,
) -> Result<String, Vec<String>> {
    let dir = page.parent().unwrap_or(Path::new(""));
    let mut broken = Vec::new();
    let mut fenced = false;
    let mut out = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            fenced = !fenced;
        }
        if fenced {
            out.push_str(line);
            continue;
        }
        let mut last = 0;
        for (range, target) in link_targets(line) {
            if target.contains("://") || target.starts_with('#') || target.starts_with("mailto:") {
                continue;
            }
            let (path, anchor) = target.split_once('#').map_or((target, ""), |(p, a)| (p, a));
            let Some(resolved) = normalize(&dir.join(path)) else {
                broken.push(format!(
                    "{}: {target} leaves the repository",
                    page.display()
                ));
                continue;
            };
            if pages.contains(&resolved) {
                continue;
            }
            let Some(is_dir) = exists(&resolved) else {
                broken.push(format!("{}: {target}", page.display()));
                continue;
            };
            let kind = if is_dir { "tree" } else { "blob" };
            let hash = if anchor.is_empty() { "" } else { "#" };
            let replacement = format!("{GITHUB}/{kind}/main/{}{hash}{anchor}", resolved.display());
            out.push_str(&line[last..range.start]);
            out.push_str(&replacement);
            last = range.end;
        }
        out.push_str(&line[last..]);
    }
    if broken.is_empty() {
        Ok(out)
    } else {
        Err(broken)
    }
}

/// Resolve `.` and `..` without touching the filesystem. `None` when the path
/// climbs above the repository root.
fn normalize(p: &Path) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::Normal(n) => out.push(n),
            Component::ParentDir => {
                if !out.pop() {
                    return None;
                }
            }
            Component::CurDir => {}
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pages() -> BTreeSet<PathBuf> {
        summary_pages("[Intro](README.md)\n- [Ops](docs/operations.md)\n- [Helm](helm/README.md)\n")
    }

    fn exists(p: &Path) -> Option<bool> {
        match p.to_str()? {
            "helm/buildkit-controller/values.yaml" | "LICENSE" => Some(false),
            "examples" => Some(true),
            _ => None,
        }
    }

    #[test]
    fn summary_lists_repository_paths() {
        let want: BTreeSet<PathBuf> = ["README.md", "docs/operations.md", "helm/README.md"]
            .into_iter()
            .map(PathBuf::from)
            .collect();
        assert_eq!(pages(), want);
    }

    #[test]
    fn page_links_stay_and_file_links_go_to_github() {
        let text = "See [helm](../helm/README.md#values), [ops](#sizing), \
                    [values](../helm/buildkit-controller/values.yaml \"t\") and [x](https://a.b/c).\n";
        let got = rewrite(text, Path::new("docs/operations.md"), &pages(), exists).unwrap();
        assert_eq!(
            got,
            "See [helm](../helm/README.md#values), [ops](#sizing), \
             [values](https://github.com/smrt-devops/buildkit-controller/blob/main/helm/buildkit-controller/values.yaml \"t\") \
             and [x](https://a.b/c).\n"
        );
    }

    #[test]
    fn directories_use_tree_urls() {
        let got = rewrite("[ex](examples)\n", Path::new("README.md"), &pages(), exists).unwrap();
        assert_eq!(
            got,
            "[ex](https://github.com/smrt-devops/buildkit-controller/tree/main/examples)\n"
        );
    }

    #[test]
    fn broken_and_escaping_links_are_all_reported() {
        let text = "[a](missing.md) [b](../../up.md)\n";
        let err = rewrite(text, Path::new("docs/operations.md"), &pages(), exists).unwrap_err();
        assert_eq!(err.len(), 2, "{err:?}");
    }

    #[test]
    fn fenced_code_is_left_alone() {
        let text = "```md\n[a](missing.md)\n```\n[l](../LICENSE)\n";
        let got = rewrite(text, Path::new("docs/operations.md"), &pages(), exists).unwrap();
        assert!(got.starts_with("```md\n[a](missing.md)\n```\n"));
        assert!(got.ends_with("blob/main/LICENSE)\n"));
    }
}
