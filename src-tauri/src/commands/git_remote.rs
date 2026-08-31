use crate::blocking_with_timeout;
use crate::error::AppError;
use crate::git::types::*;
use git2::Repository;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use tauri::Emitter;

#[tauri::command]
pub async fn cmd_git_remotes(mod_path: String) -> Result<Vec<GitRemoteInfo>, AppError> {
    crate::blocking(move || {
        let repo =
            Repository::open(&mod_path).map_err(|e| format!("Failed to open repository: {e}"))?;

        let remote_names = repo
            .remotes()
            .map_err(|e| format!("Failed to list remotes: {e}"))?;

        let mut remotes = Vec::new();
        for name in remote_names.iter().flatten() {
            let remote = repo
                .find_remote(name)
                .map_err(|e| format!("Failed to find remote '{name}': {e}"))?;

            remotes.push(GitRemoteInfo {
                name: name.to_string(),
                url: remote.url().unwrap_or("").to_string(),
                push_url: remote.pushurl().map(String::from),
            });
        }

        Ok(remotes)
    })
    .await
}

#[tauri::command]
pub async fn cmd_git_add_remote(
    mod_path: String,
    name: String,
    url: String,
) -> Result<(), AppError> {
    crate::blocking(move || {
        let repo =
            Repository::open(&mod_path).map_err(|e| format!("Failed to open repository: {e}"))?;

        repo.remote(&name, &url)
            .map_err(|e| format!("Failed to add remote '{name}': {e}"))?;

        Ok(())
    })
    .await
}

#[tauri::command]
pub async fn cmd_git_remove_remote(mod_path: String, name: String) -> Result<(), AppError> {
    crate::blocking(move || {
        let repo =
            Repository::open(&mod_path).map_err(|e| format!("Failed to open repository: {e}"))?;

        repo.remote_delete(&name)
            .map_err(|e| format!("Failed to remove remote '{name}': {e}"))?;

        Ok(())
    })
    .await
}

#[tauri::command]
pub async fn cmd_git_fetch(
    app: tauri::AppHandle,
    mod_path: String,
    remote_name: Option<String>,
) -> Result<u32, AppError> {
    blocking_with_timeout(std::time::Duration::from_secs(300), move || {
        let repo = Repository::open(&mod_path)
            .map_err(|e| format!("Failed to open repository: {e}"))?;

        let rname = remote_name.as_deref().unwrap_or("origin");
        let mut remote = repo
            .find_remote(rname)
            .map_err(|e| format!("Remote '{rname}' not found: {e}"))?;

        let received = Arc::new(AtomicU32::new(0));
        let received_clone = Arc::clone(&received);

        let mut callbacks = git2::RemoteCallbacks::new();
        callbacks.credentials(|url, username, allowed| {
            crate::git::credentials::https_credentials_callback(url, username, allowed)
        });
        callbacks.transfer_progress(move |stats| {
            received_clone.store(stats.received_objects() as u32, Ordering::Relaxed);
            let _ = app.emit(
                "git://progress",
                serde_json::json!({
                    "operation": "Fetching",
                    "current": stats.received_objects(),
                    "total": stats.total_objects(),
                    "message": format!("Receiving objects: {}/{}", stats.received_objects(), stats.total_objects())
                }),
            );
            true
        });

        let mut fetch_opts = git2::FetchOptions::new();
        fetch_opts.remote_callbacks(callbacks);

        remote
            .fetch(&[] as &[&str], Some(&mut fetch_opts), None)
            .map_err(|e| format!("Failed to fetch from '{rname}': {e}"))?;

        Ok(received.load(Ordering::Relaxed))
    })
    .await
}

#[tauri::command]
pub async fn cmd_git_pull(
    app: tauri::AppHandle,
    mod_path: String,
    remote_name: Option<String>,
) -> Result<GitPullResult, AppError> {
    blocking_with_timeout(std::time::Duration::from_secs(300), move || {
        let repo = Repository::open(&mod_path)
            .map_err(|e| format!("Failed to open repository: {e}"))?;

        let rname = remote_name.as_deref().unwrap_or("origin");
        let mut remote = repo
            .find_remote(rname)
            .map_err(|e| format!("Remote '{rname}' not found: {e}"))?;

        // Step 1: Fetch
        let received = Arc::new(AtomicU32::new(0));
        let received_clone = Arc::clone(&received);
        let app_clone = app.clone();

        let mut callbacks = git2::RemoteCallbacks::new();
        callbacks.credentials(|url, username, allowed| {
            crate::git::credentials::https_credentials_callback(url, username, allowed)
        });
        callbacks.transfer_progress(move |stats| {
            received_clone.store(stats.received_objects() as u32, Ordering::Relaxed);
            let _ = app_clone.emit(
                "git://progress",
                serde_json::json!({
                    "operation": "Fetching",
                    "current": stats.received_objects(),
                    "total": stats.total_objects(),
                    "message": format!("Receiving objects: {}/{}", stats.received_objects(), stats.total_objects())
                }),
            );
            true
        });

        let mut fetch_opts = git2::FetchOptions::new();
        fetch_opts.remote_callbacks(callbacks);

        remote
            .fetch(&[] as &[&str], Some(&mut fetch_opts), None)
            .map_err(|e| format!("Failed to fetch from '{rname}': {e}"))?;

        let fetched_objects = received.load(Ordering::Relaxed);

        // Step 2: Find upstream tracking branch
        let head = repo
            .head()
            .map_err(|e| format!("Failed to get HEAD: {e}"))?;
        let branch_name = head
            .shorthand()
            .ok_or_else(|| "HEAD is detached".to_string())?;
        let local_branch = repo
            .find_branch(branch_name, git2::BranchType::Local)
            .map_err(|e| format!("Failed to find local branch '{branch_name}': {e}"))?;
        let upstream = local_branch
            .upstream()
            .map_err(|e| format!("No upstream branch configured: {e}"))?;
        let upstream_oid = upstream
            .get()
            .target()
            .ok_or_else(|| "No upstream target".to_string())?;
        let annotated = repo
            .find_annotated_commit(upstream_oid)
            .map_err(|e| format!("Failed to find annotated commit: {e}"))?;

        // Step 3: Merge analysis
        let (analysis, _preference) = repo
            .merge_analysis(&[&annotated])
            .map_err(|e| format!("Merge analysis failed: {e}"))?;

        if analysis.is_up_to_date() {
            return Ok(GitPullResult {
                merge_result: GitMergeResult {
                    fast_forward: false,
                    conflicts: vec![],
                    merge_commit: None,
                },
                fetched_objects,
            });
        }

        if analysis.is_fast_forward() {
            let refname = repo
                .head()
                .map_err(|e| format!("Failed to get HEAD: {e}"))?
                .name()
                .ok_or_else(|| "HEAD has no name".to_string())?
                .to_string();

            repo.find_reference(&refname)
                .map_err(|e| format!("Failed to find reference: {e}"))?
                .set_target(upstream_oid, "Fast-forward pull")
                .map_err(|e| format!("Failed to update reference: {e}"))?;

            repo.set_head(&refname)
                .map_err(|e| format!("Failed to set HEAD: {e}"))?;

            repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
                .map_err(|e| format!("Failed to checkout: {e}"))?;

            return Ok(GitPullResult {
                merge_result: GitMergeResult {
                    fast_forward: true,
                    conflicts: vec![],
                    merge_commit: None,
                },
                fetched_objects,
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

            return Ok(GitPullResult {
                merge_result: GitMergeResult {
                    fast_forward: false,
                    conflicts,
                    merge_commit: None,
                },
                fetched_objects,
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
            .find_commit(upstream_oid)
            .map_err(|e| format!("Failed to find upstream commit: {e}"))?;

        let sig = crate::git::credentials::build_signature("", "")?;

        let merge_oid = repo
            .commit(
                Some("HEAD"),
                &sig,
                &sig,
                &format!("Merge remote-tracking branch '{rname}/{branch_name}'"),
                &tree,
                &[&head_commit, &merge_commit_obj],
            )
            .map_err(|e| format!("Failed to create merge commit: {e}"))?;

        repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
            .map_err(|e| format!("Failed to checkout after merge: {e}"))?;

        repo.cleanup_state()
            .map_err(|e| format!("Failed to cleanup merge state: {e}"))?;

        Ok(GitPullResult {
            merge_result: GitMergeResult {
                fast_forward: false,
                conflicts: vec![],
                merge_commit: Some(merge_oid.to_string()),
            },
            fetched_objects,
        })
    })
    .await
}

#[tauri::command]
pub async fn cmd_git_push(
    app: tauri::AppHandle,
    mod_path: String,
    remote_name: Option<String>,
    force: Option<bool>,
) -> Result<(), AppError> {
    blocking_with_timeout(std::time::Duration::from_secs(300), move || {
        let repo =
            Repository::open(&mod_path).map_err(|e| format!("Failed to open repository: {e}"))?;

        let rname = remote_name.as_deref().unwrap_or("origin");
        let force = force.unwrap_or(false);

        let mut remote = repo
            .find_remote(rname)
            .map_err(|e| format!("Remote '{rname}' not found: {e}"))?;

        let head = repo
            .head()
            .map_err(|e| format!("Failed to get HEAD: {e}"))?;
        let branch = head
            .shorthand()
            .ok_or_else(|| "HEAD is detached".to_string())?;

        let refspec = if force {
            format!("+refs/heads/{branch}:refs/heads/{branch}")
        } else {
            format!("refs/heads/{branch}:refs/heads/{branch}")
        };

        let push_err = Arc::new(std::sync::Mutex::new(None::<String>));
        let push_err_clone = Arc::clone(&push_err);

        let mut callbacks = git2::RemoteCallbacks::new();
        callbacks.credentials(|url, username, allowed| {
            crate::git::credentials::https_credentials_callback(url, username, allowed)
        });
        callbacks.push_transfer_progress(move |current, total, _bytes| {
            let _ = app.emit(
                "git://progress",
                serde_json::json!({
                    "operation": "Pushing",
                    "current": current,
                    "total": total,
                    "message": format!("Sending objects: {current}/{total}")
                }),
            );
        });
        callbacks.push_update_reference(move |_refname, status| {
            if let Some(msg) = status {
                *push_err_clone.lock().unwrap() = Some(msg.to_string());
            }
            Ok(())
        });

        let mut push_opts = git2::PushOptions::new();
        push_opts.remote_callbacks(callbacks);

        remote
            .push(&[&refspec], Some(&mut push_opts))
            .map_err(|e| format!("Failed to push to '{rname}': {e}"))?;

        if let Some(err) = push_err.lock().unwrap().take() {
            return Err(format!("Push rejected: {err}"));
        }

        Ok(())
    })
    .await
}
