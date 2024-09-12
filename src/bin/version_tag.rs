use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::Parser;
use colored::Colorize;
use git2::{Error, Oid, Repository};

#[derive(Parser, Debug)]
#[command(author, version, name = "vtag", about = "Create git version tags for a Rust project")]
struct Args {
    /// Optional git repository path. Defaults to current directory.
    path: Option<String>,

    /// Only print information without creating or pushing tags
    #[arg(short, long)]
    dryrun: bool,

    /// Push tags to remote
    #[arg(short, long)]
    push: bool,

    /// Verbose output
    #[arg(short, long)]
    verbose: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let repo_path = cli_tools::resolve_input_path(args.path.as_deref())?;
    if !repo_path.is_dir() {
        anyhow::bail!("Input path needs to be a git repository directory")
    }
    if !directory_has_cargo_toml(&repo_path) {
        anyhow::bail!("No Cargo.toml found in the input path")
    }
    version_tag(&repo_path, args.push, args.dryrun, args.verbose)
}

fn version_tag(repo_path: &PathBuf, push: bool, dryrun: bool, verbose: bool) -> Result<()> {
    if verbose {
        let name = get_package_name(repo_path).unwrap_or_else(|| cli_tools::path_to_string_relative(repo_path));
        println!("{}", format!("Creating version tags for {name}").magenta().bold());
    }

    let repo = Repository::discover(repo_path)?;
    let mut reverse_walk = repo.revwalk()?;
    reverse_walk.push_head()?;
    reverse_walk.set_sorting(git2::Sort::TOPOLOGICAL | git2::Sort::REVERSE)?;

    let mut current_tag = String::new();
    let mut tags_to_push = Vec::new();

    // Walk through each commit that modified Cargo.toml
    for oid in reverse_walk {
        let oid = oid?;
        let commit = repo.find_commit(oid)?;
        let tree = commit.tree()?;
        if let Some(entry) = tree.get_name("Cargo.toml") {
            if let Ok(blob) = entry
                .to_object(&repo)
                .and_then(|obj| obj.into_blob().map_err(|_| Error::from_str("Not a blob")))
            {
                let content = std::str::from_utf8(blob.content()).unwrap_or_default();
                match content.parse::<toml::Value>() {
                    Ok(toml_value) => {
                        if let Some(version_number) = toml_value
                            .get("package")
                            .and_then(|pkg| pkg.get("version"))
                            .and_then(toml::Value::as_str)
                        {
                            if current_tag == version_number {
                                if verbose {
                                    println!("{}", format!("Skip {}: {}", version_number, commit.id()).yellow());
                                }
                                continue;
                            }

                            current_tag = version_number.to_string();
                            println!("{}", version_number.to_string().bold());

                            let version_tag = format!("v{version_number}");
                            if tag_name_exists(&repo, &version_tag)? {
                                println!("{}", format!("Tag {version_tag} already exists, skipping...").yellow());
                            } else {
                                tag_version(&repo, &version_tag, version_number, commit.id(), dryrun)?;
                            }
                            if push {
                                tags_to_push.push(version_tag);
                            }
                        }
                    }
                    Err(e) => {
                        println!("Failed to parse TOML: {e}");
                    }
                }
            }
        };
    }

    // Push all the collected tags at once
    if push && !tags_to_push.is_empty() {
        if verbose {
            println!("Pushing {} tags to remote", tags_to_push.len());
        }
        push_all_tags(&repo, &tags_to_push, dryrun)?;
    }

    Ok(())
}

fn tag_version(repo: &Repository, tag_name: &str, version_number: &str, oid: Oid, dryrun: bool) -> Result<()> {
    let message = format!("Version {version_number}");
    if dryrun {
        println!("Dry-run: Tag {tag_name} with message '{message}'");
        return Ok(());
    }

    let commit = repo.find_commit(oid)?;
    repo.tag(
        tag_name,
        commit.as_object(),
        &repo.signature()?,
        &format!("Version {message}"),
        false,
    )?;
    println!("Created tag: {tag_name}");
    Ok(())
}

#[allow(unused)]
/// Push a single tag to remote.
fn push_tag(repo: &Repository, tag_name: &str, dryrun: bool) -> Result<()> {
    if dryrun {
        println!("Dry-run: Push tag {tag_name}");
        return Ok(());
    }

    let mut remote = repo.find_remote("origin")?;
    let refspec = format!("refs/tags/{tag_name}");
    remote.push(&[&refspec], None)?;
    println!("{}", format!("Pushed tag: {tag_name}").green());
    Ok(())
}

/// Push multiple tags to remote.
fn push_all_tags(repo: &Repository, tags: &[String], dryrun: bool) -> Result<()> {
    if dryrun {
        println!("Dry-run: Push tags {tags:?}");
        return Ok(());
    }

    let mut remote = repo.find_remote("origin")?;
    let refspecs: Vec<String> = tags.iter().map(|tag| format!("refs/tags/{tag}")).collect();
    let refspec_refs: Vec<&str> = refspecs.iter().map(std::string::String::as_str).collect();

    remote.push(&refspec_refs, None)?;
    println!("Pushed tags: {tags:?}");
    Ok(())
}

/// Check if the tag already exists locally.
fn tag_name_exists(repo: &Repository, tag_name: &str) -> Result<bool> {
    for tag in repo.tag_names(Some("v*"))?.iter().flatten() {
        if tag == tag_name {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Check that the given directory path contains a Cargo.toml file,
/// meaning that the directory contains a Rust project.
fn directory_has_cargo_toml(path: &Path) -> bool {
    path.join("Cargo.toml").exists()
}

/// Read Rust package name from Cargo.toml package config.
fn get_package_name(path: &Path) -> Option<String> {
    let cargo_toml_path = path.join("Cargo.toml");
    let cargo_toml_content = std::fs::read_to_string(cargo_toml_path).ok()?;
    let cargo_toml: toml::Value = toml::from_str(&cargo_toml_content).ok()?;
    cargo_toml
        .get("package")?
        .get("name")?
        .as_str()
        .map(std::string::ToString::to_string)
}
