extern crate failure;
extern crate git2;
extern crate memchr;
#[macro_use]
extern crate slog;

mod owned;
mod stack;
mod commute;

use std::io::Write;
use stack::WorkingStackOptions;

pub struct Config<'a> {
    pub dry_run: bool,
    pub force: bool,
    pub base: Option<&'a str>,
    pub logger: &'a slog::Logger,
}

pub fn run(config: &Config) -> Result<(), failure::Error> {
    let repo = git2::Repository::open_from_env()?;
    debug!(config.logger, "repository found"; "path" => repo.path().to_str());

    let base = match config.base {
        // https://github.com/rust-lang/rfcs/issues/1815
        Some(commitish) => Some(repo.find_commit(repo.revparse_single(commitish)?.id())?),
        None => None,
    };

    let mut diff_options = Some({
        let mut ret = git2::DiffOptions::new();
        ret.context_lines(0)
            .id_abbrev(40)
            .ignore_filemode(true)
            .ignore_submodules(true);
        ret
    });

    let stack: Vec<_> = {
        let stack = stack::working_stack(&repo, base.as_ref(), config.logger)?;
        let mut diffs = Vec::with_capacity(stack.len());
        for commit in &stack {
            let diff = owned::Diff::new(&repo.diff_tree_to_tree(
                if commit.parents().len() == 0 {
                    None
                } else {
                    Some(commit.parent(0)?.tree()?)
                }.as_ref(),
                Some(&commit.tree()?),
                diff_options.as_mut(),
            )?)?;
            trace!(config.logger, "parsed commit diff";
                   "commit" => commit.id().to_string(),
                   "diff" => format!("{:?}", diff),
            );
            diffs.push(diff);
        }

        stack.into_iter().zip(diffs.into_iter()).collect()
    };

    let mut head_tree = repo.head()?.peel_to_tree()?;
    let index =
        owned::Diff::new(&repo.diff_tree_to_index(Some(&head_tree), None, diff_options.as_mut())?)?;
    trace!(config.logger, "parsed index";
           "index" => format!("{:?}", index),
    );

    let signature = repo.signature()?;
    let mut head_commit = repo.head()?.peel_to_commit()?;

    'patch: for index_patch in index.iter() {
        'hunk: for index_hunk in &index_patch.hunks {
            let mut commuted_index_hunk = index_hunk.clone();
            if index_patch.status != git2::Delta::Modified {
                debug!(config.logger, "skipped non-modified hunk";
                       "path" => String::from_utf8_lossy(index_patch.new_path.as_slice()).into_owned(),
                       "status" => format!("{:?}", index_patch.status),
                );
                continue 'patch;
            }
            let mut commuted_old_path = index_patch.old_path.as_slice();
            debug!(config.logger, "commuting hunk";
                   "path" => String::from_utf8_lossy(commuted_old_path).into_owned(),
                   "header" => format!("-{},{} +{},{}",
                                     commuted_index_hunk.removed.start,
                                     commuted_index_hunk.removed.lines.len(),
                                     commuted_index_hunk.added.start,
                                     commuted_index_hunk.added.lines.len(),
                   ),
            );

            // find the newest commit that the hunk cannot commute
            // with
            let mut dest_commit = None;
            'commit: for &(ref commit, ref diff) in &stack {
                let c_logger = config.logger.new(o!(
                    "commit" => commit.id().to_string(),
                ));
                let next_patch = match diff.by_new(commuted_old_path) {
                    Some(patch) => patch,
                    // this commit doesn't touch the hunk's file, so
                    // they trivially commute, and the next commit
                    // should be considered
                    None => {
                        debug!(c_logger, "skipped commit with no path");
                        continue 'commit;
                    }
                };
                if next_patch.status == git2::Delta::Added {
                    debug!(c_logger, "found noncommutative commit by add");
                    dest_commit = Some(commit);
                    break 'commit;
                }
                if commuted_old_path != next_patch.old_path.as_slice() {
                    debug!(c_logger, "changed commute path";
                           "path" => String::from_utf8_lossy(&next_patch.old_path).into_owned(),
                    );
                    commuted_old_path = next_patch.old_path.as_slice();
                }
                commuted_index_hunk = match commute::commute_diff_before(
                    &commuted_index_hunk,
                    &next_patch.hunks,
                ) {
                    Some(hunk) => {
                        debug!(c_logger, "commuted hunk with commit";
                               "offset" => (hunk.added.start as i64) - (commuted_index_hunk.added.start as i64),
                        );
                        hunk
                    }
                    // this commit contains a hunk that cannot
                    // commute with the hunk being absorbed
                    None => {
                        debug!(c_logger, "found noncommutative commit by conflict");
                        dest_commit = Some(commit);
                        break 'commit;
                    }
                };
            }
            let dest_commit = match dest_commit {
                Some(commit) => commit,
                // the hunk commutes with every commit in the stack,
                // so there is no commit to absorb it into
                None => {
                    debug!(config.logger, "could not find noncommutative commit");
                    continue 'hunk;
                }
            };

            if !config.dry_run {
                head_tree =
                    apply_hunk_to_tree(&repo, &head_tree, index_hunk, &index_patch.old_path)?;
                head_commit = repo.find_commit(repo.commit(
                    Some("HEAD"),
                    &signature,
                    &signature,
                    &format!("fixup! {} {}", dest_commit.id(),
                        dest_commit.summary().unwrap_or("<no message>")),
                    &head_tree,
                    &[&head_commit],
                )?)?;
                info!(config.logger, "committed";
                      "commit" => head_commit.id().to_string(),
                );
            } else {
                info!(config.logger, "would have committed";
                      "fixup" => dest_commit.id().to_string(),
                      "header" => format!("-{},{} +{},{}",
                                          index_hunk.removed.start,
                                          index_hunk.removed.lines.len(),
                                          index_hunk.added.start,
                                          index_hunk.added.lines.len(),
                      ),
                );
            }
        }
    }

    Ok(())
}

fn apply_hunk_to_tree<'repo>(
    repo: &'repo git2::Repository,
    base: &git2::Tree,
    hunk: &owned::Hunk,
    path: &[u8],
) -> Result<git2::Tree<'repo>, failure::Error> {
    let mut treebuilder = repo.treebuilder(Some(base))?;
    let path_str = String::from_utf8_lossy(path);

    let complex_path : Vec<_> = path_str.split("/").collect();
    if complex_path.len() > 1 {
        let rest = complex_path[1..].join("/");
        let (result_tree_id, entry_filemode) = {
            let entry = treebuilder
                .get(complex_path[0])?
                .ok_or_else(|| 
                            failure::err_msg(format!("couldn't find sub tree entry for path {}", 
                                                     path[0])))?;
            let tree = repo.find_tree(entry.id())
                .map_err(|_|
                         failure::err_msg(format!("oid for {} is not a tree", 
                                                  path[0])))?;
            let result_tree = apply_hunk_to_tree(repo, &tree, hunk, rest.as_bytes())?;
            (result_tree.id(), entry.filemode())
        };

        treebuilder.insert(complex_path[0], result_tree_id, entry_filemode)?;
        return Ok(repo.find_tree(treebuilder.write()?)?)
    }

    let (blob, mode) = {
        let entry = treebuilder
            .get(path)?
            .ok_or_else(|| 
                failure::err_msg(format!("couldn't find leaf tree entry for path {}", 
                                         String::from_utf8_lossy(path))))?;
        (repo.find_blob(entry.id())?, entry.filemode())
    };

    // TODO: convert path to OsStr and pass it during blob_writer
    // creation, to get gitattributes handling (note that converting
    // &[u8] to &std::path::Path is only possible on unixy platforms)
    let mut blobwriter = repo.blob_writer(None)?;
    let old_content = blob.content();
    let (old_start, _, _, _) = hunk.anchors();

    // first, write the lines from the old content that are above the
    // hunk
    let old_content = {
        let (pre, post) = old_content.split_at(skip_past_nth(b'\n', old_content, old_start));
        blobwriter.write_all(pre)?;
        post
    };
    // next, write the added side of the hunk
    for line in &*hunk.added.lines {
        blobwriter.write_all(line)?;
    }
    // if this hunk removed lines from the old content, those must be
    // skipped
    let old_content = &old_content[skip_past_nth(b'\n', old_content, hunk.removed.lines.len())..];
    // finally, write the remaining lines of the old content
    blobwriter.write_all(old_content)?;

    treebuilder.insert(path, blobwriter.commit()?, mode)?;
    Ok(repo.find_tree(treebuilder.write()?)?)
}

fn skip_past_nth(needle: u8, haystack: &[u8], n: usize) -> usize {
    if n == 0 {
        return 0;
    }

    // TODO: is fuse necessary here?
    memchr::Memchr::new(needle, haystack)
        .fuse()
        .skip(n - 1)
        .next()
        .map(|x| x + 1)
        .unwrap_or(haystack.len())
}
