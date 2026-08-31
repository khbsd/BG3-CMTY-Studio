use crate::blocking;
use crate::error::AppError;
use crate::git::types::*;
use git2::{BranchType, Repository, StatusOptions};

#[tauri::command]
pub async fn cmd_git_branches(mod_path: String) -> Result<Vec<GitBranchInfo>, AppError> {
    blocking(move || {
        let repo =
            Repository::open(&mod_path).map_err(|e| format!("Failed to open repository: {e}"))?;

        let head_ref = repo.head().ok().and_then(|r| r.target());

        let mut branches = Vec::new();

        for branch_type in &[BranchType::Local, BranchType::Remote] {
            let iter = repo
                .branches(Some(*branch_type))
                .map_err(|e| format!("Failed to list branches: {e}"))?;

            for entry in iter {
                let (branch, bt) = entry.map_err(|e| format!("Failed to read branch: {e}"))?;

                let name = branch
                    .name()
                    .map_err(|e| format!("Failed to read branch name: {e}"))?
                    .unwrap_or("")
                    .to_string();

                let is_remote = bt == BranchType::Remote;

                let tip_oid = branch.get().target();

                let is_current = !is_remote && head_ref.is_some() && tip_oid == head_ref;

                let upstream = if !is_remote {
                    branch
                        .upstream()
                        .ok()
                        .and_then(|u| u.name().ok().flatten().map(String::from))
                } else {
                    None
                };

                let (ahead, behind) = if !is_remote {
                    if let (Some(local_oid), Some(upstream_branch)) =
                        (tip_oid, branch.upstream().ok())
                    {
                        if let Some(upstream_oid) = upstream_branch.get().target() {
                            repo.graph_ahead_behind(local_oid, upstream_oid)
                                .map(|(a, b)| (a as u32, b as u32))
                                .unwrap_or((0, 0))
                        } else {
                            (0, 0)
                        }
                    } else {
                        (0, 0)
                    }
                } else {
                    (0, 0)
                };

                branches.push(GitBranchInfo {
                    name,
                    is_current,
                    is_remote,
                    upstream,
                    ahead,
                    behind,
                    last_commit_oid: tip_oid.map(|o| o.to_string()),
                });
            }
        }

        Ok(branches)
    })
    .await
}

#[tauri::command]
pub async fn cmd_git_checkout(mod_path: String, branch: String) -> Result<(), AppError> {
    blocking(move || {
        let repo =
            Repository::open(&mod_path).map_err(|e| format!("Failed to open repository: {e}"))?;

        // Check for uncommitted changes
        let mut opts = StatusOptions::new();
        opts.include_untracked(true)
            .recurse_untracked_dirs(true)
            .include_unmodified(false);
        let statuses = repo
            .statuses(Some(&mut opts))
            .map_err(|e| format!("Failed to read status: {e}"))?;
        if !statuses.is_empty() {
            return Err(
                "Cannot switch branches: you have uncommitted changes. Commit or stash them first."
                    .to_string(),
            );
        }

        let branch_ref = repo
            .find_branch(&branch, BranchType::Local)
            .map_err(|e| format!("Branch '{branch}' not found: {e}"))?;

        let refname = branch_ref
            .get()
            .name()
            .ok_or_else(|| format!("Branch '{branch}' has invalid UTF-8 refname"))?
            .to_string();

        repo.set_head(&refname)
            .map_err(|e| format!("Failed to set HEAD: {e}"))?;

        repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
            .map_err(|e| format!("Failed to checkout: {e}"))?;

        Ok(())
    })
    .await
}

#[tauri::command]
pub async fn cmd_git_create_branch(
    mod_path: String,
    name: String,
    from: Option<String>,
) -> Result<GitBranchInfo, AppError> {
    blocking(move || {
        let repo =
            Repository::open(&mod_path).map_err(|e| format!("Failed to open repository: {e}"))?;

        let commit = if let Some(ref rev) = from {
            let obj = repo
                .revparse_single(rev)
                .map_err(|e| format!("Failed to resolve '{rev}': {e}"))?;
            obj.peel_to_commit()
                .map_err(|e| format!("'{rev}' is not a commit: {e}"))?
        } else {
            let head = repo
                .head()
                .map_err(|e| format!("Failed to resolve HEAD: {e}"))?;
            let oid = head
                .target()
                .ok_or_else(|| "HEAD is not a direct reference".to_string())?;
            repo.find_commit(oid)
                .map_err(|e| format!("Failed to find HEAD commit: {e}"))?
        };

        let branch = repo
            .branch(&name, &commit, false)
            .map_err(|e| format!("Failed to create branch '{name}': {e}"))?;

        let tip_oid = branch.get().target();

        Ok(GitBranchInfo {
            name: name.clone(),
            is_current: false,
            is_remote: false,
            upstream: None,
            ahead: 0,
            behind: 0,
            last_commit_oid: tip_oid.map(|o| o.to_string()),
        })
    })
    .await
}

#[tauri::command]
pub async fn cmd_git_delete_branch(mod_path: String, name: String) -> Result<(), AppError> {
    blocking(move || {
        let repo =
            Repository::open(&mod_path).map_err(|e| format!("Failed to open repository: {e}"))?;

        let mut branch = repo
            .find_branch(&name, BranchType::Local)
            .map_err(|e| format!("Branch '{name}' not found: {e}"))?;

        if branch.is_head() {
            return Err("Cannot delete the current branch".to_string());
        }

        branch
            .delete()
            .map_err(|e| format!("Failed to delete branch '{name}': {e}"))?;

        Ok(())
    })
    .await
}

#[tauri::command]
pub async fn cmd_git_merge(mod_path: String, branch: String) -> Result<GitMergeResult, AppError> {
    blocking(move || {
        let repo =
            Repository::open(&mod_path).map_err(|e| format!("Failed to open repository: {e}"))?;

        let branch_ref = repo
            .find_branch(&branch, BranchType::Local)
            .map_err(|e| format!("Branch '{branch}' not found: {e}"))?;

        let branch_oid = branch_ref
            .get()
            .target()
            .ok_or_else(|| format!("Branch '{branch}' has no target commit"))?;

        let annotated = repo
            .find_annotated_commit(branch_oid)
            .map_err(|e| format!("Failed to find annotated commit: {e}"))?;

        let (analysis, _preference) = repo
            .merge_analysis(&[&annotated])
            .map_err(|e| format!("Merge analysis failed: {e}"))?;

        if analysis.is_up_to_date() {
            return Ok(GitMergeResult {
                fast_forward: false,
                conflicts: vec![],
                merge_commit: None,
            });
        }

        if analysis.is_fast_forward() {
            // Fast-forward: move HEAD to the target commit
            let refname = repo
                .head()
                .map_err(|e| format!("Failed to get HEAD: {e}"))?
                .name()
                .ok_or_else(|| "HEAD has no name".to_string())?
                .to_string();

            repo.find_reference(&refname)
                .map_err(|e| format!("Failed to find reference: {e}"))?
                .set_target(branch_oid, &format!("Fast-forward to {branch}"))
                .map_err(|e| format!("Failed to update reference: {e}"))?;

            repo.set_head(&refname)
                .map_err(|e| format!("Failed to set HEAD: {e}"))?;

            repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
                .map_err(|e| format!("Failed to checkout: {e}"))?;

            return Ok(GitMergeResult {
                fast_forward: true,
                conflicts: vec![],
                merge_commit: None,
            });
        }

        // Normal merge
        repo.merge(&[&annotated], None, None)
            .map_err(|e| format!("Merge failed: {e}"))?;

        let index = repo
            .index()
            .map_err(|e| format!("Failed to get index: {e}"))?;

        if index.has_conflicts() {
            let conflicts: Vec<String> = index
                .conflicts()
                .map_err(|e| format!("Failed to read conflicts: {e}"))?
                .filter_map(|c| {
                    c.ok().and_then(|entry| {
                        entry
                            .our
                            .or(entry.their)
                            .or(entry.ancestor)
                            .and_then(|ie| String::from_utf8(ie.path).ok())
                    })
                })
                .collect();

            repo.cleanup_state()
                .map_err(|e| format!("Failed to cleanup merge state: {e}"))?;

            return Ok(GitMergeResult {
                fast_forward: false,
                conflicts,
                merge_commit: None,
            });
        }

        // No conflicts — create merge commit
        let mut index = repo
            .index()
            .map_err(|e| format!("Failed to get index: {e}"))?;
        let tree_oid = index
            .write_tree()
            .map_err(|e| format!("Failed to write tree: {e}"))?;
        let tree = repo
            .find_tree(tree_oid)
            .map_err(|e| format!("Failed to find tree: {e}"))?;

        let head_commit = repo
            .head()
            .map_err(|e| format!("Failed to get HEAD: {e}"))?
            .peel_to_commit()
            .map_err(|e| format!("Failed to peel HEAD to commit: {e}"))?;

        let merge_commit_obj = repo
            .find_commit(branch_oid)
            .map_err(|e| format!("Failed to find merge commit: {e}"))?;

        let sig = crate::git::credentials::build_signature("", "")?;

        let merge_oid = repo
            .commit(
                Some("HEAD"),
                &sig,
                &sig,
                &format!("Merge branch '{branch}'"),
                &tree,
                &[&head_commit, &merge_commit_obj],
            )
            .map_err(|e| format!("Failed to create merge commit: {e}"))?;

        repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
            .map_err(|e| format!("Failed to checkout after merge: {e}"))?;

        repo.cleanup_state()
            .map_err(|e| format!("Failed to cleanup merge state: {e}"))?;

        Ok(GitMergeResult {
            fast_forward: false,
            conflicts: vec![],
            merge_commit: Some(merge_oid.to_string()),
        })
    })
    .await
}
