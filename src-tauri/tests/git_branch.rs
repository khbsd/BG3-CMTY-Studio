//! Integration tests for git branch operations using git2 directly.
//!
//! Tests create, checkout, delete, merge, and detached HEAD scenarios.

use git2::{BranchType, Repository};
use std::fs;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Create a repo with one commit on the default branch.
fn init_repo_with_commit(dir: &TempDir) -> Repository {
    let repo = Repository::init(dir.path()).unwrap();
    let sig = git2::Signature::now("Test User", "test@example.com").unwrap();

    fs::write(dir.path().join("README.md"), "# Test\n").unwrap();

    let mut index = repo.index().unwrap();
    index
        .add_all(["*"], git2::IndexAddOption::DEFAULT, None)
        .unwrap();
    index.write().unwrap();

    let tree_oid = index.write_tree().unwrap();

    {
        let tree = repo.find_tree(tree_oid).unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "Initial commit", &tree, &[])
            .unwrap();
    }

    repo
}

/// Create a commit on the current branch, returning its OID.
fn make_commit(repo: &Repository, dir: &TempDir, filename: &str, message: &str) -> git2::Oid {
    let sig = git2::Signature::now("Test User", "test@example.com").unwrap();
    fs::write(dir.path().join(filename), message).unwrap();

    let mut index = repo.index().unwrap();
    index
        .add_all(["*"], git2::IndexAddOption::DEFAULT, None)
        .unwrap();
    index.write().unwrap();

    let tree_oid = index.write_tree().unwrap();
    let tree = repo.find_tree(tree_oid).unwrap();

    let head_commit = repo.head().unwrap().peel_to_commit().unwrap();

    repo.commit(Some("HEAD"), &sig, &sig, message, &tree, &[&head_commit])
        .unwrap()
}

// ---------------------------------------------------------------------------
// Create branch
// ---------------------------------------------------------------------------

#[test]
fn create_branch_appears_in_list() {
    let dir = TempDir::new().unwrap();
    let repo = init_repo_with_commit(&dir);

    let head_commit = repo.head().unwrap().peel_to_commit().unwrap();
    repo.branch("feature", &head_commit, false).unwrap();

    let branches: Vec<String> = repo
        .branches(Some(BranchType::Local))
        .unwrap()
        .filter_map(|b| {
            let (branch, _) = b.ok()?;
            branch.name().ok().flatten().map(String::from)
        })
        .collect();

    assert!(branches.contains(&"feature".to_string()));
}

// ---------------------------------------------------------------------------
// Checkout branch
// ---------------------------------------------------------------------------

#[test]
fn checkout_branch_updates_head() {
    let dir = TempDir::new().unwrap();
    let repo = init_repo_with_commit(&dir);

    let head_commit = repo.head().unwrap().peel_to_commit().unwrap();
    repo.branch("feature", &head_commit, false).unwrap();

    // Checkout "feature"
    let branch_ref = repo.find_branch("feature", BranchType::Local).unwrap();
    let refname = branch_ref.get().name().unwrap().to_string();
    repo.set_head(&refname).unwrap();
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap();

    // HEAD should now point to "feature"
    let head = repo.head().unwrap();
    assert_eq!(head.shorthand().unwrap(), "feature");
}

// ---------------------------------------------------------------------------
// Delete branch
// ---------------------------------------------------------------------------

#[test]
fn delete_branch_removes_it() {
    let dir = TempDir::new().unwrap();
    let repo = init_repo_with_commit(&dir);

    let head_commit = repo.head().unwrap().peel_to_commit().unwrap();
    repo.branch("to-delete", &head_commit, false).unwrap();

    // Verify it exists
    assert!(repo.find_branch("to-delete", BranchType::Local).is_ok());

    // Delete it
    let mut branch = repo.find_branch("to-delete", BranchType::Local).unwrap();
    branch.delete().unwrap();

    // Should no longer be found
    assert!(repo.find_branch("to-delete", BranchType::Local).is_err());
}

// ---------------------------------------------------------------------------
// Merge (fast-forward)
// ---------------------------------------------------------------------------

#[test]
fn fast_forward_merge() {
    let dir = TempDir::new().unwrap();
    let repo = init_repo_with_commit(&dir);

    // Create "feature" from HEAD
    let head_commit = repo.head().unwrap().peel_to_commit().unwrap();
    repo.branch("feature", &head_commit, false).unwrap();

    // Checkout "feature" and add a commit
    let branch_ref = repo.find_branch("feature", BranchType::Local).unwrap();
    let refname = branch_ref.get().name().unwrap().to_string();
    repo.set_head(&refname).unwrap();
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap();

    let feature_oid = make_commit(&repo, &dir, "feature.txt", "feature work");

    // Checkout back to the default branch (master/main)
    let default_branch_name = {
        let branches: Vec<String> = repo
            .branches(Some(BranchType::Local))
            .unwrap()
            .filter_map(|b| {
                let (branch, _) = b.ok()?;
                let name = branch.name().ok().flatten()?.to_string();
                if name != "feature" {
                    Some(name)
                } else {
                    None
                }
            })
            .collect();
        branches[0].clone()
    };
    let main_ref = repo
        .find_branch(&default_branch_name, BranchType::Local)
        .unwrap();
    let main_refname = main_ref.get().name().unwrap().to_string();
    repo.set_head(&main_refname).unwrap();
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap();

    // Merge analysis should indicate fast-forward
    let annotated = repo.find_annotated_commit(feature_oid).unwrap();
    let (analysis, _) = repo.merge_analysis(&[&annotated]).unwrap();
    assert!(analysis.is_fast_forward());

    // Perform fast-forward by updating the reference
    let head_refname = repo.head().unwrap().name().unwrap().to_string();
    repo.find_reference(&head_refname)
        .unwrap()
        .set_target(feature_oid, "fast-forward merge")
        .unwrap();
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap();

    // main should now have the feature commit
    let head_oid = repo.head().unwrap().target().unwrap();
    assert_eq!(head_oid, feature_oid);
}

// ---------------------------------------------------------------------------
// Merge (conflict)
// ---------------------------------------------------------------------------

#[test]
fn merge_conflict_detected() {
    let dir = TempDir::new().unwrap();
    let repo = init_repo_with_commit(&dir);

    // Create "feature" branch from HEAD
    let head_commit = repo.head().unwrap().peel_to_commit().unwrap();
    repo.branch("feature", &head_commit, false).unwrap();

    // Commit a change on the default branch
    make_commit(&repo, &dir, "README.md", "main change");

    // Checkout "feature" and commit a conflicting change to the same file
    let feature_ref = repo.find_branch("feature", BranchType::Local).unwrap();
    let feature_refname = feature_ref.get().name().unwrap().to_string();
    repo.set_head(&feature_refname).unwrap();
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap();

    let feature_oid = make_commit(&repo, &dir, "README.md", "feature change");

    // Checkout default branch again
    let default_name = {
        let branches: Vec<String> = repo
            .branches(Some(BranchType::Local))
            .unwrap()
            .filter_map(|b| {
                let (branch, _) = b.ok()?;
                let name = branch.name().ok().flatten()?.to_string();
                if name != "feature" {
                    Some(name)
                } else {
                    None
                }
            })
            .collect();
        branches[0].clone()
    };
    let main_ref = repo.find_branch(&default_name, BranchType::Local).unwrap();
    let main_refname = main_ref.get().name().unwrap().to_string();
    repo.set_head(&main_refname).unwrap();
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap();

    // Attempt merge — should produce conflicts
    let annotated = repo.find_annotated_commit(feature_oid).unwrap();
    let (analysis, _) = repo.merge_analysis(&[&annotated]).unwrap();
    assert!(analysis.is_normal());

    repo.merge(&[&annotated], None, None).unwrap();

    let index = repo.index().unwrap();
    assert!(index.has_conflicts(), "index should have conflicts");

    // Cleanup merge state
    repo.cleanup_state().unwrap();
}

// ---------------------------------------------------------------------------
// Detached HEAD
// ---------------------------------------------------------------------------

#[test]
fn detached_head_after_checkout_oid() {
    let dir = TempDir::new().unwrap();
    let repo = init_repo_with_commit(&dir);

    let head_oid = repo.head().unwrap().target().unwrap();

    // Detach HEAD to a specific OID
    repo.set_head_detached(head_oid).unwrap();

    assert!(repo.head_detached().unwrap());
}

// ---------------------------------------------------------------------------
// Cannot delete current branch
// ---------------------------------------------------------------------------

#[test]
fn cannot_delete_current_branch() {
    let dir = TempDir::new().unwrap();
    let repo = init_repo_with_commit(&dir);

    // HEAD points to the default branch — find its name
    let head_name = repo.head().unwrap().shorthand().unwrap().to_string();

    let mut branch = repo.find_branch(&head_name, BranchType::Local).unwrap();
    assert!(branch.is_head());

    // git2 should error when trying to delete the current branch
    let result = branch.delete();
    assert!(result.is_err());
}
