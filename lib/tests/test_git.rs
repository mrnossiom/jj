// Copyright 2020 The Jujutsu Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::collections::BTreeMap;
use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::iter;
use std::num::NonZeroU32;
use std::path::Path;
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::Arc;
use std::sync::Barrier;
use std::thread;

use assert_matches::assert_matches;
use git2::Oid;
use itertools::Itertools;
use jj_lib::backend::BackendError;
use jj_lib::backend::ChangeId;
use jj_lib::backend::CommitId;
use jj_lib::backend::MillisSinceEpoch;
use jj_lib::backend::Signature;
use jj_lib::backend::Timestamp;
use jj_lib::commit::Commit;
use jj_lib::commit_builder::CommitBuilder;
use jj_lib::git;
use jj_lib::git::FailedRefExportReason;
use jj_lib::git::GitBranchPushTargets;
use jj_lib::git::GitFetch;
use jj_lib::git::GitFetchError;
use jj_lib::git::GitFetchStats;
use jj_lib::git::GitImportError;
use jj_lib::git::GitPushError;
use jj_lib::git::GitRefUpdate;
use jj_lib::git::RefName;
use jj_lib::git::SubmoduleConfig;
use jj_lib::git_backend::GitBackend;
use jj_lib::object_id::ObjectId;
use jj_lib::op_store::BookmarkTarget;
use jj_lib::op_store::RefTarget;
use jj_lib::op_store::RemoteRef;
use jj_lib::op_store::RemoteRefState;
use jj_lib::refs::BookmarkPushUpdate;
use jj_lib::repo::MutableRepo;
use jj_lib::repo::ReadonlyRepo;
use jj_lib::repo::Repo;
use jj_lib::repo_path::RepoPath;
use jj_lib::settings::GitSettings;
use jj_lib::settings::UserSettings;
use jj_lib::signing::Signer;
use jj_lib::str_util::StringPattern;
use jj_lib::workspace::Workspace;
use maplit::btreemap;
use maplit::hashset;
use tempfile::TempDir;
use test_case::test_case;
use testutils::commit_transactions;
use testutils::create_random_commit;
use testutils::write_random_commit;
use testutils::TestRepo;
use testutils::TestRepoBackend;

fn empty_git_commit<'r>(
    git_repo: &'r git2::Repository,
    ref_name: &str,
    parents: &[&git2::Commit],
) -> git2::Commit<'r> {
    let signature = git2::Signature::now("Someone", "someone@example.com").unwrap();
    let empty_tree_id = Oid::from_str("4b825dc642cb6eb9a060e54bf8d69288fbee4904").unwrap();
    let empty_tree = git_repo.find_tree(empty_tree_id).unwrap();
    let oid = git_repo
        .commit(
            Some(ref_name),
            &signature,
            &signature,
            &format!("random commit {}", rand::random::<u32>()),
            &empty_tree,
            parents,
        )
        .unwrap();
    git_repo.find_commit(oid).unwrap()
}

fn jj_id(commit: &git2::Commit) -> CommitId {
    CommitId::from_bytes(commit.id().as_bytes())
}

fn git_id(commit: &Commit) -> Oid {
    Oid::from_bytes(commit.id().as_bytes()).unwrap()
}

fn get_git_backend(repo: &Arc<ReadonlyRepo>) -> &GitBackend {
    repo.store()
        .backend_impl()
        .downcast_ref::<GitBackend>()
        .unwrap()
}

fn get_git_repo(repo: &Arc<ReadonlyRepo>) -> git2::Repository {
    get_git_backend(repo).open_git_repo().unwrap()
}

fn git_fetch(
    mut_repo: &mut MutableRepo,
    git_repo: &git2::Repository,
    remote_name: &str,
    branch_names: &[StringPattern],
    callbacks: git::RemoteCallbacks<'_>,
    git_settings: &GitSettings,
    depth: Option<NonZeroU32>,
) -> Result<GitFetchStats, GitFetchError> {
    let mut git_fetch = GitFetch::new(mut_repo, git_repo, git_settings);
    let default_branch = git_fetch.fetch(remote_name, branch_names, callbacks, depth)?;
    let import_stats = git_fetch.import_refs()?;
    let stats = GitFetchStats {
        default_branch,
        import_stats,
    };
    Ok(stats)
}

#[test]
fn test_import_refs() {
    let settings = testutils::user_settings();
    let git_settings = GitSettings {
        auto_local_bookmark: true,
        ..Default::default()
    };
    let test_repo = TestRepo::init_with_backend(TestRepoBackend::Git);
    let repo = &test_repo.repo;
    let git_repo = get_git_repo(repo);

    let commit1 = empty_git_commit(&git_repo, "refs/heads/main", &[]);
    git_ref(&git_repo, "refs/remotes/origin/main", commit1.id());
    let commit2 = empty_git_commit(&git_repo, "refs/heads/main", &[&commit1]);
    let commit3 = empty_git_commit(&git_repo, "refs/heads/feature1", &[&commit2]);
    let commit4 = empty_git_commit(&git_repo, "refs/heads/feature2", &[&commit2]);
    let commit5 = empty_git_commit(&git_repo, "refs/tags/v1.0", &[&commit1]);
    let commit6 = empty_git_commit(&git_repo, "refs/remotes/origin/feature3", &[&commit1]);
    // Should not be imported
    empty_git_commit(&git_repo, "refs/notes/x", &[&commit2]);
    empty_git_commit(&git_repo, "refs/remotes/origin/HEAD", &[&commit2]);

    git_repo.set_head("refs/heads/main").unwrap();

    let mut tx = repo.start_transaction(&settings);
    git::import_head(tx.repo_mut()).unwrap();
    let stats = git::import_refs(tx.repo_mut(), &git_settings).unwrap();
    tx.repo_mut().rebase_descendants(&settings).unwrap();
    let repo = tx.commit("test").unwrap();
    let view = repo.view();

    assert!(stats.abandoned_commits.is_empty());
    let expected_heads = hashset! {
        jj_id(&commit3),
        jj_id(&commit4),
        jj_id(&commit5),
        jj_id(&commit6)
    };
    assert_eq!(*view.heads(), expected_heads);

    assert_eq!(view.bookmarks().count(), 4);
    assert_eq!(
        view.get_local_bookmark("main"),
        &RefTarget::normal(jj_id(&commit2))
    );
    assert_eq!(
        view.get_remote_bookmark("main", "git"),
        &RemoteRef {
            target: RefTarget::normal(jj_id(&commit2)),
            state: RemoteRefState::Tracking,
        },
    );
    assert_eq!(
        view.get_remote_bookmark("main", "origin"),
        &RemoteRef {
            target: RefTarget::normal(jj_id(&commit1)),
            state: RemoteRefState::Tracking,
        },
    );
    assert_eq!(
        view.get_local_bookmark("feature1"),
        &RefTarget::normal(jj_id(&commit3))
    );
    assert_eq!(
        view.get_remote_bookmark("feature1", "git"),
        &RemoteRef {
            target: RefTarget::normal(jj_id(&commit3)),
            state: RemoteRefState::Tracking,
        },
    );
    assert!(view.get_remote_bookmark("feature1", "origin").is_absent());
    assert_eq!(
        view.get_local_bookmark("feature2"),
        &RefTarget::normal(jj_id(&commit4))
    );
    assert_eq!(
        view.get_remote_bookmark("feature2", "git"),
        &RemoteRef {
            target: RefTarget::normal(jj_id(&commit4)),
            state: RemoteRefState::Tracking,
        },
    );
    assert!(view.get_remote_bookmark("feature2", "origin").is_absent());
    assert_eq!(
        view.get_local_bookmark("feature3"),
        &RefTarget::normal(jj_id(&commit6))
    );
    assert!(view.get_remote_bookmark("feature3", "git").is_absent());
    assert_eq!(
        view.get_remote_bookmark("feature3", "origin"),
        &RemoteRef {
            target: RefTarget::normal(jj_id(&commit6)),
            state: RemoteRefState::Tracking,
        },
    );

    assert_eq!(view.get_tag("v1.0"), &RefTarget::normal(jj_id(&commit5)));

    assert_eq!(view.git_refs().len(), 6);
    assert_eq!(
        view.get_git_ref("refs/heads/main"),
        &RefTarget::normal(jj_id(&commit2))
    );
    assert_eq!(
        view.get_git_ref("refs/heads/feature1"),
        &RefTarget::normal(jj_id(&commit3))
    );
    assert_eq!(
        view.get_git_ref("refs/heads/feature2"),
        &RefTarget::normal(jj_id(&commit4))
    );
    assert_eq!(
        view.get_git_ref("refs/remotes/origin/main"),
        &RefTarget::normal(jj_id(&commit1))
    );
    assert_eq!(
        view.get_git_ref("refs/remotes/origin/feature3"),
        &RefTarget::normal(jj_id(&commit6))
    );
    assert_eq!(
        view.get_git_ref("refs/tags/v1.0"),
        &RefTarget::normal(jj_id(&commit5))
    );
    assert_eq!(view.git_head(), &RefTarget::normal(jj_id(&commit2)));
}

#[test]
fn test_import_refs_reimport() {
    let settings = testutils::user_settings();
    let git_settings = GitSettings {
        auto_local_bookmark: true,
        ..Default::default()
    };
    let test_workspace = TestRepo::init_with_backend(TestRepoBackend::Git);
    let repo = &test_workspace.repo;
    let git_repo = get_git_repo(repo);

    let commit1 = empty_git_commit(&git_repo, "refs/heads/main", &[]);
    git_ref(&git_repo, "refs/remotes/origin/main", commit1.id());
    let commit2 = empty_git_commit(&git_repo, "refs/heads/main", &[&commit1]);
    let commit3 = empty_git_commit(&git_repo, "refs/heads/feature1", &[&commit2]);
    let commit4 = empty_git_commit(&git_repo, "refs/heads/feature2", &[&commit2]);
    let pgp_key_oid = git_repo.blob(b"my PGP key").unwrap();
    git_repo
        .reference("refs/tags/my-gpg-key", pgp_key_oid, false, "")
        .unwrap();

    let mut tx = repo.start_transaction(&settings);
    let stats = git::import_refs(tx.repo_mut(), &git_settings).unwrap();
    tx.repo_mut().rebase_descendants(&settings).unwrap();
    let repo = tx.commit("test").unwrap();

    assert!(stats.abandoned_commits.is_empty());
    let expected_heads = hashset! {
            jj_id(&commit3),
            jj_id(&commit4),
    };
    let view = repo.view();
    assert_eq!(*view.heads(), expected_heads);

    // Delete feature1 and rewrite feature2
    delete_git_ref(&git_repo, "refs/heads/feature1");
    delete_git_ref(&git_repo, "refs/heads/feature2");
    let commit5 = empty_git_commit(&git_repo, "refs/heads/feature2", &[&commit2]);

    // Also modify feature2 on the jj side
    let mut tx = repo.start_transaction(&settings);
    let commit6 = create_random_commit(tx.repo_mut(), &settings)
        .set_parents(vec![jj_id(&commit2)])
        .write()
        .unwrap();
    tx.repo_mut()
        .set_local_bookmark_target("feature2", RefTarget::normal(commit6.id().clone()));
    let repo = tx.commit("test").unwrap();

    let mut tx = repo.start_transaction(&settings);
    let stats = git::import_refs(tx.repo_mut(), &git_settings).unwrap();
    tx.repo_mut().rebase_descendants(&settings).unwrap();
    let repo = tx.commit("test").unwrap();

    assert_eq!(
        // The order is unstable just because we import heads from Git repo.
        HashSet::from_iter(stats.abandoned_commits),
        hashset! {
            jj_id(&commit4),
            jj_id(&commit3),
        },
    );
    let view = repo.view();
    let expected_heads = hashset! {
            jj_id(&commit5),
            commit6.id().clone(),
    };
    assert_eq!(*view.heads(), expected_heads);

    assert_eq!(view.bookmarks().count(), 2);
    let commit1_target = RefTarget::normal(jj_id(&commit1));
    let commit2_target = RefTarget::normal(jj_id(&commit2));
    assert_eq!(
        view.get_local_bookmark("main"),
        &RefTarget::normal(jj_id(&commit2))
    );
    assert_eq!(
        view.get_remote_bookmark("main", "git"),
        &RemoteRef {
            target: RefTarget::normal(jj_id(&commit2)),
            state: RemoteRefState::Tracking,
        },
    );
    assert_eq!(
        view.get_remote_bookmark("main", "origin"),
        &RemoteRef {
            target: commit1_target.clone(),
            state: RemoteRefState::Tracking,
        },
    );
    assert_eq!(
        view.get_local_bookmark("feature2"),
        &RefTarget::from_legacy_form([jj_id(&commit4)], [commit6.id().clone(), jj_id(&commit5)])
    );
    assert_eq!(
        view.get_remote_bookmark("feature2", "git"),
        &RemoteRef {
            target: RefTarget::normal(jj_id(&commit5)),
            state: RemoteRefState::Tracking,
        },
    );
    assert!(view.get_remote_bookmark("feature2", "origin").is_absent());

    assert!(view.tags().is_empty());

    assert_eq!(view.git_refs().len(), 3);
    assert_eq!(view.get_git_ref("refs/heads/main"), &commit2_target);
    assert_eq!(
        view.get_git_ref("refs/remotes/origin/main"),
        &commit1_target
    );
    let commit5_target = RefTarget::normal(jj_id(&commit5));
    assert_eq!(view.get_git_ref("refs/heads/feature2"), &commit5_target);
}

#[test]
fn test_import_refs_reimport_head_removed() {
    // Test that re-importing refs doesn't cause a deleted head to come back
    let settings = testutils::user_settings();
    let git_settings = GitSettings::default();
    let test_repo = TestRepo::init_with_backend(TestRepoBackend::Git);
    let repo = &test_repo.repo;
    let git_repo = get_git_repo(repo);

    let commit = empty_git_commit(&git_repo, "refs/heads/main", &[]);
    let mut tx = repo.start_transaction(&settings);
    git::import_refs(tx.repo_mut(), &git_settings).unwrap();
    tx.repo_mut().rebase_descendants(&settings).unwrap();
    let commit_id = jj_id(&commit);
    // Test the setup
    assert!(tx.repo_mut().view().heads().contains(&commit_id));

    // Remove the head and re-import
    tx.repo_mut().remove_head(&commit_id);
    git::import_refs(tx.repo_mut(), &git_settings).unwrap();
    tx.repo_mut().rebase_descendants(&settings).unwrap();
    assert!(!tx.repo_mut().view().heads().contains(&commit_id));
}

#[test]
fn test_import_refs_reimport_git_head_does_not_count() {
    // Test that if a bookmark is removed, the corresponding commit is abandoned
    // no matter if the Git HEAD points to the commit (or a descendant of it.)
    let settings = testutils::user_settings();
    let git_settings = GitSettings::default();
    let test_repo = TestRepo::init_with_backend(TestRepoBackend::Git);
    let repo = &test_repo.repo;
    let git_repo = get_git_repo(repo);

    let commit = empty_git_commit(&git_repo, "refs/heads/main", &[]);
    git_repo.set_head_detached(commit.id()).unwrap();

    let mut tx = repo.start_transaction(&settings);
    git::import_head(tx.repo_mut()).unwrap();
    git::import_refs(tx.repo_mut(), &git_settings).unwrap();
    tx.repo_mut().rebase_descendants(&settings).unwrap();

    // Delete the bookmark and re-import. The commit should still be there since
    // HEAD points to it
    git_repo
        .find_reference("refs/heads/main")
        .unwrap()
        .delete()
        .unwrap();
    git::import_head(tx.repo_mut()).unwrap();
    git::import_refs(tx.repo_mut(), &git_settings).unwrap();
    tx.repo_mut().rebase_descendants(&settings).unwrap();
    assert!(!tx.repo_mut().view().heads().contains(&jj_id(&commit)));
}

#[test]
fn test_import_refs_reimport_git_head_without_ref() {
    // Simulate external `git checkout` in colocated repo, from anonymous bookmark.
    let settings = testutils::user_settings();
    let git_settings = GitSettings::default();
    let test_repo = TestRepo::init_with_backend(TestRepoBackend::Git);
    let repo = &test_repo.repo;
    let git_repo = get_git_repo(repo);

    // First, HEAD points to commit1.
    let mut tx = repo.start_transaction(&settings);
    let commit1 = write_random_commit(tx.repo_mut(), &settings);
    let commit2 = write_random_commit(tx.repo_mut(), &settings);
    git_repo.set_head_detached(git_id(&commit1)).unwrap();

    // Import HEAD.
    git::import_head(tx.repo_mut()).unwrap();
    git::import_refs(tx.repo_mut(), &git_settings).unwrap();
    tx.repo_mut().rebase_descendants(&settings).unwrap();
    assert!(tx.repo_mut().view().heads().contains(commit1.id()));
    assert!(tx.repo_mut().view().heads().contains(commit2.id()));

    // Move HEAD to commit2 (by e.g. `git checkout` command)
    git_repo.set_head_detached(git_id(&commit2)).unwrap();

    // Reimport HEAD, which doesn't abandon the old HEAD branch because jj thinks it
    // would be moved by `git checkout` command. This isn't always true because the
    // detached HEAD commit could be rewritten by e.g. `git commit --amend` command,
    // but it should be safer than abandoning old checkout branch.
    git::import_head(tx.repo_mut()).unwrap();
    git::import_refs(tx.repo_mut(), &git_settings).unwrap();
    tx.repo_mut().rebase_descendants(&settings).unwrap();
    assert!(tx.repo_mut().view().heads().contains(commit1.id()));
    assert!(tx.repo_mut().view().heads().contains(commit2.id()));
}

#[test]
fn test_import_refs_reimport_git_head_with_moved_ref() {
    // Simulate external history rewriting in colocated repo.
    let settings = testutils::user_settings();
    let git_settings = GitSettings::default();
    let test_repo = TestRepo::init_with_backend(TestRepoBackend::Git);
    let repo = &test_repo.repo;
    let git_repo = get_git_repo(repo);

    // First, both HEAD and main point to commit1.
    let mut tx = repo.start_transaction(&settings);
    let commit1 = write_random_commit(tx.repo_mut(), &settings);
    let commit2 = write_random_commit(tx.repo_mut(), &settings);
    git_repo
        .reference("refs/heads/main", git_id(&commit1), true, "test")
        .unwrap();
    git_repo.set_head_detached(git_id(&commit1)).unwrap();

    // Import HEAD and main.
    git::import_head(tx.repo_mut()).unwrap();
    git::import_refs(tx.repo_mut(), &git_settings).unwrap();
    tx.repo_mut().rebase_descendants(&settings).unwrap();
    assert!(tx.repo_mut().view().heads().contains(commit1.id()));
    assert!(tx.repo_mut().view().heads().contains(commit2.id()));

    // Move both HEAD and main to commit2 (by e.g. `git commit --amend` command)
    git_repo
        .reference("refs/heads/main", git_id(&commit2), true, "test")
        .unwrap();
    git_repo.set_head_detached(git_id(&commit2)).unwrap();

    // Reimport HEAD and main, which abandons the old main branch.
    git::import_head(tx.repo_mut()).unwrap();
    git::import_refs(tx.repo_mut(), &git_settings).unwrap();
    tx.repo_mut().rebase_descendants(&settings).unwrap();
    assert!(!tx.repo_mut().view().heads().contains(commit1.id()));
    assert!(tx.repo_mut().view().heads().contains(commit2.id()));
    // Reimport HEAD and main, which abandons the old main bookmark.
    git::import_head(tx.repo_mut()).unwrap();
    git::import_refs(tx.repo_mut(), &git_settings).unwrap();
    tx.repo_mut().rebase_descendants(&settings).unwrap();
    assert!(!tx.repo_mut().view().heads().contains(commit1.id()));
    assert!(tx.repo_mut().view().heads().contains(commit2.id()));
}

#[test]
fn test_import_refs_reimport_with_deleted_remote_ref() {
    let settings = testutils::user_settings();
    let git_settings = GitSettings {
        auto_local_bookmark: true,
        ..Default::default()
    };
    let test_workspace = TestRepo::init_with_backend(TestRepoBackend::Git);
    let repo = &test_workspace.repo;
    let git_repo = get_git_repo(repo);

    let commit_base = empty_git_commit(&git_repo, "refs/heads/main", &[]);
    let commit_main = empty_git_commit(&git_repo, "refs/heads/main", &[&commit_base]);
    let commit_remote_only = empty_git_commit(
        &git_repo,
        "refs/remotes/origin/feature-remote-only",
        &[&commit_base],
    );
    let commit_remote_and_local = empty_git_commit(
        &git_repo,
        "refs/remotes/origin/feature-remote-and-local",
        &[&commit_base],
    );
    git_ref(
        &git_repo,
        "refs/heads/feature-remote-and-local",
        commit_remote_and_local.id(),
    );

    let mut tx = repo.start_transaction(&settings);
    git::import_refs(tx.repo_mut(), &git_settings).unwrap();
    tx.repo_mut().rebase_descendants(&settings).unwrap();
    let repo = tx.commit("test").unwrap();

    let expected_heads = hashset! {
            jj_id(&commit_main),
            jj_id(&commit_remote_only),
            jj_id(&commit_remote_and_local),
    };
    let view = repo.view();
    assert_eq!(*view.heads(), expected_heads);
    assert_eq!(view.bookmarks().count(), 3);
    // Even though the git repo does not have a local bookmark for
    // `feature-remote-only`, jj creates one. This follows the model explained
    // in docs/bookmarks.md.
    assert_eq!(
        view.get_local_bookmark("feature-remote-only"),
        &RefTarget::normal(jj_id(&commit_remote_only))
    );
    assert!(view
        .get_remote_bookmark("feature-remote-only", "git")
        .is_absent());
    assert_eq!(
        view.get_remote_bookmark("feature-remote-only", "origin"),
        &RemoteRef {
            target: RefTarget::normal(jj_id(&commit_remote_only)),
            state: RemoteRefState::Tracking,
        },
    );
    assert_eq!(
        view.get_local_bookmark("feature-remote-and-local"),
        &RefTarget::normal(jj_id(&commit_remote_and_local))
    );
    assert_eq!(
        view.get_remote_bookmark("feature-remote-and-local", "git"),
        &RemoteRef {
            target: RefTarget::normal(jj_id(&commit_remote_and_local)),
            state: RemoteRefState::Tracking,
        },
    );
    assert_eq!(
        view.get_remote_bookmark("feature-remote-and-local", "origin"),
        &RemoteRef {
            target: RefTarget::normal(jj_id(&commit_remote_and_local)),
            state: RemoteRefState::Tracking,
        },
    );
    assert!(view.get_local_bookmark("main").is_present()); // bookmark #3 of 3

    // Simulate fetching from a remote where feature-remote-only and
    // feature-remote-and-local bookmarks were deleted. This leads to the
    // following import deleting the corresponding local bookmarks.
    delete_git_ref(&git_repo, "refs/remotes/origin/feature-remote-only");
    delete_git_ref(&git_repo, "refs/remotes/origin/feature-remote-and-local");

    let mut tx = repo.start_transaction(&settings);
    git::import_refs(tx.repo_mut(), &git_settings).unwrap();
    tx.repo_mut().rebase_descendants(&settings).unwrap();
    let repo = tx.commit("test").unwrap();

    let view = repo.view();
    // The local bookmarks were indeed deleted
    assert_eq!(view.bookmarks().count(), 2);
    assert!(view.get_local_bookmark("main").is_present());
    assert!(view.get_local_bookmark("feature-remote-only").is_absent());
    assert!(view
        .get_remote_bookmark("feature-remote-only", "origin")
        .is_absent());
    assert!(view
        .get_local_bookmark("feature-remote-and-local")
        .is_absent());
    assert_eq!(
        view.get_remote_bookmark("feature-remote-and-local", "git"),
        &RemoteRef {
            target: RefTarget::normal(jj_id(&commit_remote_and_local)),
            state: RemoteRefState::Tracking,
        },
    );
    assert!(view
        .get_remote_bookmark("feature-remote-and-local", "origin")
        .is_absent());
    let expected_heads = hashset! {
            jj_id(&commit_main),
            // Neither commit_remote_only nor commit_remote_and_local should be
            // listed as a head. commit_remote_only was never affected by #864,
            // but commit_remote_and_local was.
    };
    assert_eq!(*view.heads(), expected_heads);
}

/// This test is nearly identical to the previous one, except the bookmarks are
/// moved sideways instead of being deleted.
#[test]
fn test_import_refs_reimport_with_moved_remote_ref() {
    let settings = testutils::user_settings();
    let git_settings = GitSettings {
        auto_local_bookmark: true,
        ..Default::default()
    };
    let test_workspace = TestRepo::init_with_backend(TestRepoBackend::Git);
    let repo = &test_workspace.repo;
    let git_repo = get_git_repo(repo);

    let commit_base = empty_git_commit(&git_repo, "refs/heads/main", &[]);
    let commit_main = empty_git_commit(&git_repo, "refs/heads/main", &[&commit_base]);
    let commit_remote_only = empty_git_commit(
        &git_repo,
        "refs/remotes/origin/feature-remote-only",
        &[&commit_base],
    );
    let commit_remote_and_local = empty_git_commit(
        &git_repo,
        "refs/remotes/origin/feature-remote-and-local",
        &[&commit_base],
    );
    git_ref(
        &git_repo,
        "refs/heads/feature-remote-and-local",
        commit_remote_and_local.id(),
    );

    let mut tx = repo.start_transaction(&settings);
    git::import_refs(tx.repo_mut(), &git_settings).unwrap();
    tx.repo_mut().rebase_descendants(&settings).unwrap();
    let repo = tx.commit("test").unwrap();

    let expected_heads = hashset! {
            jj_id(&commit_main),
            jj_id(dbg!(&commit_remote_only)),
            jj_id(dbg!(&commit_remote_and_local)),
    };
    let view = repo.view();
    assert_eq!(*view.heads(), expected_heads);
    assert_eq!(view.bookmarks().count(), 3);
    // Even though the git repo does not have a local bookmark for
    // `feature-remote-only`, jj creates one. This follows the model explained
    // in docs/bookmarks.md.
    assert_eq!(
        view.get_local_bookmark("feature-remote-only"),
        &RefTarget::normal(jj_id(&commit_remote_only))
    );
    assert!(view
        .get_remote_bookmark("feature-remote-only", "git")
        .is_absent());
    assert_eq!(
        view.get_remote_bookmark("feature-remote-only", "origin"),
        &RemoteRef {
            target: RefTarget::normal(jj_id(&commit_remote_only)),
            state: RemoteRefState::Tracking,
        },
    );
    assert_eq!(
        view.get_local_bookmark("feature-remote-and-local"),
        &RefTarget::normal(jj_id(&commit_remote_and_local))
    );
    assert_eq!(
        view.get_remote_bookmark("feature-remote-and-local", "git"),
        &RemoteRef {
            target: RefTarget::normal(jj_id(&commit_remote_and_local)),
            state: RemoteRefState::Tracking,
        },
    );
    assert_eq!(
        view.get_remote_bookmark("feature-remote-and-local", "origin"),
        &RemoteRef {
            target: RefTarget::normal(jj_id(&commit_remote_and_local)),
            state: RemoteRefState::Tracking,
        },
    );
    assert!(view.get_local_bookmark("main").is_present()); // bookmark #3 of 3

    // Simulate fetching from a remote where feature-remote-only and
    // feature-remote-and-local bookmarks were moved. This leads to the
    // following import moving the corresponding local bookmarks.
    delete_git_ref(&git_repo, "refs/remotes/origin/feature-remote-only");
    delete_git_ref(&git_repo, "refs/remotes/origin/feature-remote-and-local");
    let new_commit_remote_only = empty_git_commit(
        &git_repo,
        "refs/remotes/origin/feature-remote-only",
        &[&commit_base],
    );
    let new_commit_remote_and_local = empty_git_commit(
        &git_repo,
        "refs/remotes/origin/feature-remote-and-local",
        &[&commit_base],
    );

    let mut tx = repo.start_transaction(&settings);
    git::import_refs(tx.repo_mut(), &git_settings).unwrap();
    tx.repo_mut().rebase_descendants(&settings).unwrap();
    let repo = tx.commit("test").unwrap();

    let view = repo.view();
    assert_eq!(view.bookmarks().count(), 3);
    // The local bookmarks are moved
    assert_eq!(
        view.get_local_bookmark("feature-remote-only"),
        &RefTarget::normal(jj_id(&new_commit_remote_only))
    );
    assert!(view
        .get_remote_bookmark("feature-remote-only", "git")
        .is_absent());
    assert_eq!(
        view.get_remote_bookmark("feature-remote-only", "origin"),
        &RemoteRef {
            target: RefTarget::normal(jj_id(&new_commit_remote_only)),
            state: RemoteRefState::Tracking,
        },
    );
    assert_eq!(
        view.get_local_bookmark("feature-remote-and-local"),
        &RefTarget::normal(jj_id(&new_commit_remote_and_local))
    );
    assert_eq!(
        view.get_remote_bookmark("feature-remote-and-local", "git"),
        &RemoteRef {
            target: RefTarget::normal(jj_id(&commit_remote_and_local)),
            state: RemoteRefState::Tracking,
        },
    );
    assert_eq!(
        view.get_remote_bookmark("feature-remote-and-local", "origin"),
        &RemoteRef {
            target: RefTarget::normal(jj_id(&new_commit_remote_and_local)),
            state: RemoteRefState::Tracking,
        },
    );
    assert!(view.get_local_bookmark("main").is_present()); // bookmark #3 of 3
    let expected_heads = hashset! {
            jj_id(&commit_main),
            jj_id(&new_commit_remote_and_local),
            jj_id(&new_commit_remote_only),
            // Neither commit_remote_only nor commit_remote_and_local should be
            // listed as a head. commit_remote_only was never affected by #864,
            // but commit_remote_and_local was.
    };
    assert_eq!(*view.heads(), expected_heads);
}

#[test]
fn test_import_refs_reimport_with_moved_untracked_remote_ref() {
    let settings = testutils::user_settings();
    let git_settings = GitSettings {
        auto_local_bookmark: false,
        ..Default::default()
    };
    let test_workspace = TestRepo::init_with_backend(TestRepoBackend::Git);
    let repo = &test_workspace.repo;
    let git_repo = get_git_repo(repo);

    // The base commit doesn't have a reference.
    let remote_ref_name = "refs/remotes/origin/feature";
    let commit_base = empty_git_commit(&git_repo, remote_ref_name, &[]);
    let commit_remote_t0 = empty_git_commit(&git_repo, remote_ref_name, &[&commit_base]);
    let mut tx = repo.start_transaction(&settings);
    git::import_refs(tx.repo_mut(), &git_settings).unwrap();
    tx.repo_mut().rebase_descendants(&settings).unwrap();
    let repo = tx.commit("test").unwrap();
    let view = repo.view();

    assert_eq!(*view.heads(), hashset! { jj_id(&commit_remote_t0) });
    assert_eq!(view.local_bookmarks().count(), 0);
    assert_eq!(view.all_remote_bookmarks().count(), 1);
    assert_eq!(
        view.get_remote_bookmark("feature", "origin"),
        &RemoteRef {
            target: RefTarget::normal(jj_id(&commit_remote_t0)),
            state: RemoteRefState::New,
        },
    );

    // Move the reference remotely and fetch the changes.
    delete_git_ref(&git_repo, remote_ref_name);
    let commit_remote_t1 = empty_git_commit(&git_repo, remote_ref_name, &[&commit_base]);
    let mut tx = repo.start_transaction(&settings);
    git::import_refs(tx.repo_mut(), &git_settings).unwrap();
    tx.repo_mut().rebase_descendants(&settings).unwrap();
    let repo = tx.commit("test").unwrap();
    let view = repo.view();

    // commit_remote_t0 should be abandoned, but commit_base shouldn't because
    // it's the ancestor of commit_remote_t1.
    assert_eq!(*view.heads(), hashset! { jj_id(&commit_remote_t1) });
    assert_eq!(view.local_bookmarks().count(), 0);
    assert_eq!(view.all_remote_bookmarks().count(), 1);
    assert_eq!(
        view.get_remote_bookmark("feature", "origin"),
        &RemoteRef {
            target: RefTarget::normal(jj_id(&commit_remote_t1)),
            state: RemoteRefState::New,
        },
    );
}

#[test]
fn test_import_refs_reimport_with_deleted_untracked_intermediate_remote_ref() {
    let settings = testutils::user_settings();
    let git_settings = GitSettings {
        auto_local_bookmark: false,
        ..Default::default()
    };
    let test_workspace = TestRepo::init_with_backend(TestRepoBackend::Git);
    let repo = &test_workspace.repo;
    let git_repo = get_git_repo(repo);

    // Set up linear graph:
    // o feature-b@origin
    // o feature-a@origin
    let remote_ref_name_a = "refs/remotes/origin/feature-a";
    let remote_ref_name_b = "refs/remotes/origin/feature-b";
    let commit_remote_a = empty_git_commit(&git_repo, remote_ref_name_a, &[]);
    let commit_remote_b = empty_git_commit(&git_repo, remote_ref_name_b, &[&commit_remote_a]);
    let mut tx = repo.start_transaction(&settings);
    git::import_refs(tx.repo_mut(), &git_settings).unwrap();
    tx.repo_mut().rebase_descendants(&settings).unwrap();
    let repo = tx.commit("test").unwrap();
    let view = repo.view();

    assert_eq!(*view.heads(), hashset! { jj_id(&commit_remote_b) });
    assert_eq!(view.local_bookmarks().count(), 0);
    assert_eq!(view.all_remote_bookmarks().count(), 2);
    assert_eq!(
        view.get_remote_bookmark("feature-a", "origin"),
        &RemoteRef {
            target: RefTarget::normal(jj_id(&commit_remote_a)),
            state: RemoteRefState::New,
        },
    );
    assert_eq!(
        view.get_remote_bookmark("feature-b", "origin"),
        &RemoteRef {
            target: RefTarget::normal(jj_id(&commit_remote_b)),
            state: RemoteRefState::New,
        },
    );

    // Delete feature-a remotely and fetch the changes.
    delete_git_ref(&git_repo, remote_ref_name_a);
    let mut tx = repo.start_transaction(&settings);
    git::import_refs(tx.repo_mut(), &git_settings).unwrap();
    tx.repo_mut().rebase_descendants(&settings).unwrap();
    let repo = tx.commit("test").unwrap();
    let view = repo.view();

    // No commits should be abandoned because feature-a is pinned by feature-b.
    // Otherwise, feature-b would have to be rebased locally even though the
    // user haven't made any modifications to these commits yet.
    assert_eq!(*view.heads(), hashset! { jj_id(&commit_remote_b) });
    assert_eq!(view.local_bookmarks().count(), 0);
    assert_eq!(view.all_remote_bookmarks().count(), 1);
    assert_eq!(
        view.get_remote_bookmark("feature-b", "origin"),
        &RemoteRef {
            target: RefTarget::normal(jj_id(&commit_remote_b)),
            state: RemoteRefState::New,
        },
    );
}

#[test]
fn test_import_refs_reimport_with_deleted_abandoned_untracked_remote_ref() {
    let settings = testutils::user_settings();
    let git_settings = GitSettings {
        auto_local_bookmark: false,
        ..Default::default()
    };
    let test_workspace = TestRepo::init_with_backend(TestRepoBackend::Git);
    let repo = &test_workspace.repo;
    let git_repo = get_git_repo(repo);

    // Set up linear graph:
    // o feature-b@origin
    // o feature-a@origin
    let remote_ref_name_a = "refs/remotes/origin/feature-a";
    let remote_ref_name_b = "refs/remotes/origin/feature-b";
    let commit_remote_a = empty_git_commit(&git_repo, remote_ref_name_a, &[]);
    let commit_remote_b = empty_git_commit(&git_repo, remote_ref_name_b, &[&commit_remote_a]);
    let mut tx = repo.start_transaction(&settings);
    git::import_refs(tx.repo_mut(), &git_settings).unwrap();
    tx.repo_mut().rebase_descendants(&settings).unwrap();
    let repo = tx.commit("test").unwrap();
    let view = repo.view();

    assert_eq!(*view.heads(), hashset! { jj_id(&commit_remote_b) });
    assert_eq!(view.local_bookmarks().count(), 0);
    assert_eq!(view.all_remote_bookmarks().count(), 2);
    assert_eq!(
        view.get_remote_bookmark("feature-a", "origin"),
        &RemoteRef {
            target: RefTarget::normal(jj_id(&commit_remote_a)),
            state: RemoteRefState::New,
        },
    );
    assert_eq!(
        view.get_remote_bookmark("feature-b", "origin"),
        &RemoteRef {
            target: RefTarget::normal(jj_id(&commit_remote_b)),
            state: RemoteRefState::New,
        },
    );

    // Abandon feature-b locally:
    // x feature-b@origin (hidden)
    // o feature-a@origin
    let mut tx = repo.start_transaction(&settings);
    tx.repo_mut()
        .record_abandoned_commit(jj_id(&commit_remote_b));
    tx.repo_mut().rebase_descendants(&settings).unwrap();
    let repo = tx.commit("test").unwrap();
    let view = repo.view();
    assert_eq!(*view.heads(), hashset! { jj_id(&commit_remote_a) });
    assert_eq!(view.local_bookmarks().count(), 0);
    assert_eq!(view.all_remote_bookmarks().count(), 2);

    // Delete feature-a remotely and fetch the changes.
    delete_git_ref(&git_repo, remote_ref_name_a);
    let mut tx = repo.start_transaction(&settings);
    git::import_refs(tx.repo_mut(), &git_settings).unwrap();
    tx.repo_mut().rebase_descendants(&settings).unwrap();
    let repo = tx.commit("test").unwrap();
    let view = repo.view();

    // The feature-a commit should be abandoned. Since feature-b has already
    // been abandoned, there are no descendant commits to be rebased.
    assert_eq!(
        *view.heads(),
        hashset! { repo.store().root_commit_id().clone() }
    );
    assert_eq!(view.local_bookmarks().count(), 0);
    assert_eq!(view.all_remote_bookmarks().count(), 1);
    assert_eq!(
        view.get_remote_bookmark("feature-b", "origin"),
        &RemoteRef {
            target: RefTarget::normal(jj_id(&commit_remote_b)),
            state: RemoteRefState::New,
        },
    );
}

#[test]
fn test_import_refs_reimport_git_head_with_fixed_ref() {
    // Simulate external `git checkout` in colocated repo, from named bookmark.
    let settings = testutils::user_settings();
    let git_settings = GitSettings::default();
    let test_repo = TestRepo::init_with_backend(TestRepoBackend::Git);
    let repo = &test_repo.repo;
    let git_repo = get_git_repo(repo);

    // First, both HEAD and main point to commit1.
    let mut tx = repo.start_transaction(&settings);
    let commit1 = write_random_commit(tx.repo_mut(), &settings);
    let commit2 = write_random_commit(tx.repo_mut(), &settings);
    git_repo
        .reference("refs/heads/main", git_id(&commit1), true, "test")
        .unwrap();
    git_repo.set_head_detached(git_id(&commit1)).unwrap();

    // Import HEAD and main.
    git::import_head(tx.repo_mut()).unwrap();
    git::import_refs(tx.repo_mut(), &git_settings).unwrap();
    tx.repo_mut().rebase_descendants(&settings).unwrap();
    assert!(tx.repo_mut().view().heads().contains(commit1.id()));
    assert!(tx.repo_mut().view().heads().contains(commit2.id()));

    // Move only HEAD to commit2 (by e.g. `git checkout` command)
    git_repo.set_head_detached(git_id(&commit2)).unwrap();

    // Reimport HEAD, which shouldn't abandon the old HEAD branch.
    git::import_head(tx.repo_mut()).unwrap();
    git::import_refs(tx.repo_mut(), &git_settings).unwrap();
    tx.repo_mut().rebase_descendants(&settings).unwrap();
    assert!(tx.repo_mut().view().heads().contains(commit1.id()));
    assert!(tx.repo_mut().view().heads().contains(commit2.id()));
}

#[test]
fn test_import_refs_reimport_all_from_root_removed() {
    // Test that if a chain of commits all the way from the root gets unreferenced,
    // we abandon the whole stack, but not including the root commit.
    let settings = testutils::user_settings();
    let git_settings = GitSettings::default();
    let test_repo = TestRepo::init_with_backend(TestRepoBackend::Git);
    let repo = &test_repo.repo;
    let git_repo = get_git_repo(repo);

    let commit = empty_git_commit(&git_repo, "refs/heads/main", &[]);
    let mut tx = repo.start_transaction(&settings);
    git::import_refs(tx.repo_mut(), &git_settings).unwrap();
    tx.repo_mut().rebase_descendants(&settings).unwrap();
    // Test the setup
    assert!(tx.repo_mut().view().heads().contains(&jj_id(&commit)));

    // Remove all git refs and re-import
    git_repo
        .find_reference("refs/heads/main")
        .unwrap()
        .delete()
        .unwrap();
    git::import_refs(tx.repo_mut(), &git_settings).unwrap();
    tx.repo_mut().rebase_descendants(&settings).unwrap();
    assert!(!tx.repo_mut().view().heads().contains(&jj_id(&commit)));
}

#[test]
fn test_import_refs_reimport_abandoning_disabled() {
    // Test that we don't abandoned unreachable commits if configured not to
    let settings = testutils::user_settings();
    let git_settings = GitSettings {
        abandon_unreachable_commits: false,
        ..Default::default()
    };
    let test_repo = TestRepo::init_with_backend(TestRepoBackend::Git);
    let repo = &test_repo.repo;
    let git_repo = get_git_repo(repo);

    let commit1 = empty_git_commit(&git_repo, "refs/heads/main", &[]);
    let commit2 = empty_git_commit(&git_repo, "refs/heads/delete-me", &[&commit1]);
    let mut tx = repo.start_transaction(&settings);
    git::import_refs(tx.repo_mut(), &git_settings).unwrap();
    tx.repo_mut().rebase_descendants(&settings).unwrap();
    // Test the setup
    assert!(tx.repo_mut().view().heads().contains(&jj_id(&commit2)));

    // Remove the `delete-me` bookmark and re-import
    git_repo
        .find_reference("refs/heads/delete-me")
        .unwrap()
        .delete()
        .unwrap();
    git::import_refs(tx.repo_mut(), &git_settings).unwrap();
    tx.repo_mut().rebase_descendants(&settings).unwrap();
    assert!(tx.repo_mut().view().heads().contains(&jj_id(&commit2)));
}

#[test]
fn test_import_refs_reimport_conflicted_remote_bookmark() {
    let settings = testutils::user_settings();
    let git_settings = GitSettings {
        auto_local_bookmark: true,
        ..Default::default()
    };
    let test_repo = TestRepo::init_with_backend(TestRepoBackend::Git);
    let repo = &test_repo.repo;
    let git_repo = get_git_repo(repo);

    let commit1 = empty_git_commit(&git_repo, "refs/heads/commit1", &[]);
    git_ref(&git_repo, "refs/remotes/origin/main", commit1.id());
    let mut tx1 = repo.start_transaction(&settings);
    git::import_refs(tx1.repo_mut(), &git_settings).unwrap();

    let commit2 = empty_git_commit(&git_repo, "refs/heads/commit2", &[]);
    git_ref(&git_repo, "refs/remotes/origin/main", commit2.id());
    let mut tx2 = repo.start_transaction(&settings);
    git::import_refs(tx2.repo_mut(), &git_settings).unwrap();

    // Remote bookmark can diverge by divergent operations (like `jj git fetch`)
    let repo = commit_transactions(&settings, vec![tx1, tx2]);
    assert_eq!(
        repo.view().get_git_ref("refs/remotes/origin/main"),
        &RefTarget::from_legacy_form([], [jj_id(&commit1), jj_id(&commit2)]),
    );
    assert_eq!(
        repo.view().get_remote_bookmark("main", "origin"),
        &RemoteRef {
            target: RefTarget::from_legacy_form([], [jj_id(&commit1), jj_id(&commit2)]),
            state: RemoteRefState::Tracking,
        },
    );

    // The conflict can be resolved by importing the current Git state
    let mut tx = repo.start_transaction(&settings);
    git::import_refs(tx.repo_mut(), &git_settings).unwrap();
    let repo = tx.commit("test").unwrap();
    assert_eq!(
        repo.view().get_git_ref("refs/remotes/origin/main"),
        &RefTarget::normal(jj_id(&commit2)),
    );
    assert_eq!(
        repo.view().get_remote_bookmark("main", "origin"),
        &RemoteRef {
            target: RefTarget::normal(jj_id(&commit2)),
            state: RemoteRefState::Tracking,
        },
    );
}

#[test]
fn test_import_refs_reserved_remote_name() {
    let settings = testutils::user_settings();
    let git_settings = GitSettings::default();
    let test_repo = TestRepo::init_with_backend(TestRepoBackend::Git);
    let repo = &test_repo.repo;
    let git_repo = get_git_repo(repo);

    empty_git_commit(&git_repo, "refs/remotes/git/main", &[]);

    let mut tx = repo.start_transaction(&settings);
    let result = git::import_refs(tx.repo_mut(), &git_settings);
    assert_matches!(result, Err(GitImportError::RemoteReservedForLocalGitRepo));
}

#[test]
fn test_import_some_refs() {
    let settings = testutils::user_settings();
    let git_settings = GitSettings {
        auto_local_bookmark: true,
        ..Default::default()
    };
    let test_workspace = TestRepo::init_with_backend(TestRepoBackend::Git);
    let repo = &test_workspace.repo;
    let git_repo = get_git_repo(repo);

    let commit_main = empty_git_commit(&git_repo, "refs/remotes/origin/main", &[]);
    let commit_feat1 = empty_git_commit(&git_repo, "refs/remotes/origin/feature1", &[&commit_main]);
    let commit_feat2 =
        empty_git_commit(&git_repo, "refs/remotes/origin/feature2", &[&commit_feat1]);
    let commit_feat3 =
        empty_git_commit(&git_repo, "refs/remotes/origin/feature3", &[&commit_feat1]);
    let commit_feat4 =
        empty_git_commit(&git_repo, "refs/remotes/origin/feature4", &[&commit_feat3]);
    let commit_ign = empty_git_commit(&git_repo, "refs/remotes/origin/ignored", &[]);
    // No error should be reported for the refs excluded by git_ref_filter.
    empty_git_commit(&git_repo, "refs/remotes/git/main", &[]);

    fn get_remote_bookmark(ref_name: &RefName) -> Option<&str> {
        match ref_name {
            RefName::RemoteBranch { branch, remote } if remote == "origin" => Some(branch),
            _ => None,
        }
    }

    // Import bookmarks feature1, feature2, and feature3.
    let mut tx = repo.start_transaction(&settings);
    git::import_some_refs(tx.repo_mut(), &git_settings, |ref_name| {
        get_remote_bookmark(ref_name)
            .map(|bookmark| bookmark.starts_with("feature"))
            .unwrap_or(false)
    })
    .unwrap();
    tx.repo_mut().rebase_descendants(&settings).unwrap();
    let repo = tx.commit("test").unwrap();

    // There are two heads, feature2 and feature4.
    let view = repo.view();
    let expected_heads = hashset! {
            jj_id(&commit_feat2),
            jj_id(&commit_feat4),
    };
    assert_eq!(*view.heads(), expected_heads);

    // Check that bookmarks feature[1-4] have been locally imported and are known to
    // be present on origin as well.
    assert_eq!(view.bookmarks().count(), 4);
    let commit_feat1_remote_ref = RemoteRef {
        target: RefTarget::normal(jj_id(&commit_feat1)),
        state: RemoteRefState::Tracking,
    };
    let commit_feat2_remote_ref = RemoteRef {
        target: RefTarget::normal(jj_id(&commit_feat2)),
        state: RemoteRefState::Tracking,
    };
    let commit_feat3_remote_ref = RemoteRef {
        target: RefTarget::normal(jj_id(&commit_feat3)),
        state: RemoteRefState::Tracking,
    };
    let commit_feat4_remote_ref = RemoteRef {
        target: RefTarget::normal(jj_id(&commit_feat4)),
        state: RemoteRefState::Tracking,
    };
    assert_eq!(
        view.get_local_bookmark("feature1"),
        &RefTarget::normal(jj_id(&commit_feat1))
    );
    assert_eq!(
        view.get_remote_bookmark("feature1", "origin"),
        &commit_feat1_remote_ref
    );
    assert_eq!(
        view.get_local_bookmark("feature2"),
        &RefTarget::normal(jj_id(&commit_feat2))
    );
    assert_eq!(
        view.get_remote_bookmark("feature2", "origin"),
        &commit_feat2_remote_ref
    );
    assert_eq!(
        view.get_local_bookmark("feature3"),
        &RefTarget::normal(jj_id(&commit_feat3))
    );
    assert_eq!(
        view.get_remote_bookmark("feature3", "origin"),
        &commit_feat3_remote_ref
    );
    assert_eq!(
        view.get_local_bookmark("feature4"),
        &RefTarget::normal(jj_id(&commit_feat4))
    );
    assert_eq!(
        view.get_remote_bookmark("feature4", "origin"),
        &commit_feat4_remote_ref
    );
    assert!(view.get_local_bookmark("main").is_absent());
    assert!(view.get_remote_bookmark("main", "git").is_absent());
    assert!(view.get_remote_bookmark("main", "origin").is_absent());
    assert!(!view.heads().contains(&jj_id(&commit_main)));
    assert!(view.get_local_bookmark("ignored").is_absent());
    assert!(view.get_remote_bookmark("ignored", "git").is_absent());
    assert!(view.get_remote_bookmark("ignored", "origin").is_absent());
    assert!(!view.heads().contains(&jj_id(&commit_ign)));

    // Delete bookmark feature1, feature3 and feature4 in git repository and import
    // bookmark feature2 only. That should have no impact on the jj repository.
    delete_git_ref(&git_repo, "refs/remotes/origin/feature1");
    delete_git_ref(&git_repo, "refs/remotes/origin/feature3");
    delete_git_ref(&git_repo, "refs/remotes/origin/feature4");
    let mut tx = repo.start_transaction(&settings);
    git::import_some_refs(tx.repo_mut(), &git_settings, |ref_name| {
        get_remote_bookmark(ref_name) == Some("feature2")
    })
    .unwrap();
    tx.repo_mut().rebase_descendants(&settings).unwrap();
    let repo = tx.commit("test").unwrap();

    // feature2 and feature4 will still be heads, and all four bookmarks should be
    // present.
    let view = repo.view();
    assert_eq!(view.bookmarks().count(), 4);
    assert_eq!(*view.heads(), expected_heads);

    // Import feature1: this should cause the bookmark to be deleted, but the
    // corresponding commit should stay because it is reachable from feature2.
    let mut tx = repo.start_transaction(&settings);
    git::import_some_refs(tx.repo_mut(), &git_settings, |ref_name| {
        get_remote_bookmark(ref_name) == Some("feature1")
    })
    .unwrap();
    // No descendant should be rewritten.
    assert_eq!(tx.repo_mut().rebase_descendants(&settings).unwrap(), 0);
    let repo = tx.commit("test").unwrap();

    // feature2 and feature4 should still be the heads, and all three bookmarks
    // feature2, feature3, and feature3 should exist.
    let view = repo.view();
    assert_eq!(view.bookmarks().count(), 3);
    assert_eq!(*view.heads(), expected_heads);

    // Import feature3: this should cause the bookmark to be deleted, but
    // feature4 should be left alone even though it is no longer in git.
    let mut tx = repo.start_transaction(&settings);
    git::import_some_refs(tx.repo_mut(), &git_settings, |ref_name| {
        get_remote_bookmark(ref_name) == Some("feature3")
    })
    .unwrap();
    // No descendant should be rewritten
    assert_eq!(tx.repo_mut().rebase_descendants(&settings).unwrap(), 0);
    let repo = tx.commit("test").unwrap();

    // feature2 and feature4 should still be the heads, and both bookmarks
    // should exist.
    let view = repo.view();
    assert_eq!(view.bookmarks().count(), 2);
    assert_eq!(*view.heads(), expected_heads);

    // Import feature4: both the head and the bookmark will disappear.
    let mut tx = repo.start_transaction(&settings);
    git::import_some_refs(tx.repo_mut(), &git_settings, |ref_name| {
        get_remote_bookmark(ref_name) == Some("feature4")
    })
    .unwrap();
    // No descendant should be rewritten
    assert_eq!(tx.repo_mut().rebase_descendants(&settings).unwrap(), 0);
    let repo = tx.commit("test").unwrap();

    // feature2 should now be the only head and only bookmark.
    let view = repo.view();
    assert_eq!(view.bookmarks().count(), 1);
    let expected_heads = hashset! {
            jj_id(&commit_feat2),
    };
    assert_eq!(*view.heads(), expected_heads);
}

fn git_ref(git_repo: &git2::Repository, name: &str, target: Oid) {
    git_repo.reference(name, target, true, "").unwrap();
}

fn delete_git_ref(git_repo: &git2::Repository, name: &str) {
    git_repo.find_reference(name).unwrap().delete().unwrap();
}

struct GitRepoData {
    settings: UserSettings,
    _temp_dir: TempDir,
    origin_repo: git2::Repository,
    git_repo: git2::Repository,
    repo: Arc<ReadonlyRepo>,
}

impl GitRepoData {
    fn create() -> Self {
        let settings = testutils::user_settings();
        let temp_dir = testutils::new_temp_dir();
        let origin_repo_dir = temp_dir.path().join("source");
        let origin_repo = git2::Repository::init_bare(&origin_repo_dir).unwrap();
        let git_repo_dir = temp_dir.path().join("git");
        let git_repo =
            git2::Repository::clone(origin_repo_dir.to_str().unwrap(), git_repo_dir).unwrap();
        let jj_repo_dir = temp_dir.path().join("jj");
        std::fs::create_dir(&jj_repo_dir).unwrap();
        let repo = ReadonlyRepo::init(
            &settings,
            &jj_repo_dir,
            &|settings, store_path| {
                Ok(Box::new(GitBackend::init_external(
                    settings,
                    store_path,
                    git_repo.path(),
                )?))
            },
            Signer::from_settings(&settings).unwrap(),
            ReadonlyRepo::default_op_store_initializer(),
            ReadonlyRepo::default_op_heads_store_initializer(),
            ReadonlyRepo::default_index_store_initializer(),
            ReadonlyRepo::default_submodule_store_initializer(),
        )
        .unwrap();
        Self {
            settings,
            _temp_dir: temp_dir,
            origin_repo,
            git_repo,
            repo,
        }
    }
}

#[test]
fn test_import_refs_empty_git_repo() {
    let test_data = GitRepoData::create();
    let git_settings = GitSettings::default();
    let heads_before = test_data.repo.view().heads().clone();
    let mut tx = test_data.repo.start_transaction(&test_data.settings);
    git::import_refs(tx.repo_mut(), &git_settings).unwrap();
    tx.repo_mut()
        .rebase_descendants(&test_data.settings)
        .unwrap();
    let repo = tx.commit("test").unwrap();
    assert_eq!(*repo.view().heads(), heads_before);
    assert_eq!(repo.view().bookmarks().count(), 0);
    assert_eq!(repo.view().tags().len(), 0);
    assert_eq!(repo.view().git_refs().len(), 0);
    assert_eq!(repo.view().git_head(), RefTarget::absent_ref());
}

#[test]
fn test_import_refs_missing_git_commit() {
    let settings = testutils::user_settings();
    let git_settings = GitSettings::default();
    let test_workspace = TestRepo::init_with_backend(TestRepoBackend::Git);
    let repo = &test_workspace.repo;
    let git_repo = get_git_repo(repo);

    let commit1 = empty_git_commit(&git_repo, "refs/heads/main", &[]);
    let commit2 = empty_git_commit(&git_repo, "refs/heads/main", &[&commit1]);
    let shard = hex::encode(&commit1.id().as_bytes()[..1]);
    let object_basename = hex::encode(&commit1.id().as_bytes()[1..]);
    let object_store_path = git_repo.path().join("objects");
    let object_file = object_store_path.join(&shard).join(object_basename);
    let backup_object_file = object_store_path.join(&shard).join("backup");
    assert!(object_file.exists());

    // Missing commit is ancestor of ref
    git_repo.set_head("refs/heads/unborn").unwrap();
    fs::rename(&object_file, &backup_object_file).unwrap();
    let mut tx = repo.start_transaction(&settings);
    let result = git::import_refs(tx.repo_mut(), &git_settings);
    assert_matches!(
        result,
        Err(GitImportError::MissingRefAncestor {
            ref_name,
            err: BackendError::ObjectNotFound { .. }
        }) if &ref_name == "main"
    );

    // Missing commit is ancestor of HEAD
    git_repo
        .find_reference("refs/heads/main")
        .unwrap()
        .delete()
        .unwrap();
    git_repo.set_head_detached(commit2.id()).unwrap();
    let mut tx = repo.start_transaction(&settings);
    let result = git::import_head(tx.repo_mut());
    assert_matches!(
        result,
        Err(GitImportError::MissingHeadTarget {
            id,
            err: BackendError::ObjectNotFound { .. }
        }) if id == jj_id(&commit2)
    );

    // Missing commit is pointed to by ref: the ref is ignored as we don't know
    // if the missing object is a commit or not.
    fs::rename(&backup_object_file, &object_file).unwrap();
    git_repo
        .reference("refs/heads/main", commit1.id(), true, "test")
        .unwrap();
    git_repo.set_head("refs/heads/unborn").unwrap();
    fs::rename(&object_file, &backup_object_file).unwrap();
    let mut tx = repo.start_transaction(&settings);
    let result = git::import_refs(tx.repo_mut(), &git_settings);
    assert!(result.is_ok());

    // Missing commit is pointed to by HEAD: the ref is ignored as we don't know
    // if the missing object is a commit or not.
    fs::rename(&backup_object_file, &object_file).unwrap();
    git_repo
        .find_reference("refs/heads/main")
        .unwrap()
        .delete()
        .unwrap();
    git_repo.set_head_detached(commit1.id()).unwrap();
    fs::rename(&object_file, &backup_object_file).unwrap();
    let mut tx = repo.start_transaction(&settings);
    let result = git::import_head(tx.repo_mut());
    assert!(result.is_ok());
}

#[test]
fn test_import_refs_detached_head() {
    let test_data = GitRepoData::create();
    let git_settings = GitSettings::default();
    let commit1 = empty_git_commit(&test_data.git_repo, "refs/heads/main", &[]);
    // Delete the reference. Check that the detached HEAD commit still gets added to
    // the set of heads
    test_data
        .git_repo
        .find_reference("refs/heads/main")
        .unwrap()
        .delete()
        .unwrap();
    test_data.git_repo.set_head_detached(commit1.id()).unwrap();

    let mut tx = test_data.repo.start_transaction(&test_data.settings);
    git::import_head(tx.repo_mut()).unwrap();
    git::import_refs(tx.repo_mut(), &git_settings).unwrap();
    tx.repo_mut()
        .rebase_descendants(&test_data.settings)
        .unwrap();
    let repo = tx.commit("test").unwrap();

    let expected_heads = hashset! { jj_id(&commit1) };
    assert_eq!(*repo.view().heads(), expected_heads);
    assert_eq!(repo.view().git_refs().len(), 0);
    assert_eq!(repo.view().git_head(), &RefTarget::normal(jj_id(&commit1)));
}

#[test]
fn test_export_refs_no_detach() {
    // When exporting the bookmark that's current checked out, don't detach HEAD if
    // the target already matches
    let test_data = GitRepoData::create();
    let git_settings = GitSettings::default();
    let git_repo = test_data.git_repo;
    let commit1 = empty_git_commit(&git_repo, "refs/heads/main", &[]);
    git_repo.set_head("refs/heads/main").unwrap();
    let mut tx = test_data.repo.start_transaction(&test_data.settings);
    let mut_repo = tx.repo_mut();
    git::import_head(mut_repo).unwrap();
    git::import_refs(mut_repo, &git_settings).unwrap();
    mut_repo.rebase_descendants(&test_data.settings).unwrap();

    // Do an initial export to make sure `main` is considered
    assert!(git::export_refs(mut_repo).unwrap().is_empty());
    assert_eq!(
        mut_repo.get_git_ref("refs/heads/main"),
        RefTarget::normal(jj_id(&commit1))
    );
    assert_eq!(git_repo.head().unwrap().name(), Some("refs/heads/main"));
    assert_eq!(
        git_repo.find_reference("refs/heads/main").unwrap().target(),
        Some(commit1.id())
    );
}

#[test]
fn test_export_refs_bookmark_changed() {
    // We can export a change to a bookmark
    let test_data = GitRepoData::create();
    let git_settings = GitSettings::default();
    let git_repo = test_data.git_repo;
    let commit = empty_git_commit(&git_repo, "refs/heads/main", &[]);
    git_repo
        .reference("refs/heads/feature", commit.id(), false, "test")
        .unwrap();
    git_repo.set_head("refs/heads/feature").unwrap();

    let mut tx = test_data.repo.start_transaction(&test_data.settings);
    let mut_repo = tx.repo_mut();
    git::import_head(mut_repo).unwrap();
    git::import_refs(mut_repo, &git_settings).unwrap();
    mut_repo.rebase_descendants(&test_data.settings).unwrap();
    assert!(git::export_refs(mut_repo).unwrap().is_empty());

    let new_commit = create_random_commit(mut_repo, &test_data.settings)
        .set_parents(vec![jj_id(&commit)])
        .write()
        .unwrap();
    mut_repo.set_local_bookmark_target("main", RefTarget::normal(new_commit.id().clone()));
    assert!(git::export_refs(mut_repo).unwrap().is_empty());
    assert_eq!(
        mut_repo.get_git_ref("refs/heads/main"),
        RefTarget::normal(new_commit.id().clone())
    );
    assert_eq!(
        git_repo
            .find_reference("refs/heads/main")
            .unwrap()
            .peel_to_commit()
            .unwrap()
            .id(),
        git_id(&new_commit)
    );
    // HEAD should be unchanged since its target bookmark didn't change
    assert_eq!(git_repo.head().unwrap().name(), Some("refs/heads/feature"));
}

#[test]
fn test_export_refs_current_bookmark_changed() {
    // If we update a bookmark that is checked out in the git repo, HEAD gets
    // detached
    let test_data = GitRepoData::create();
    let git_settings = GitSettings::default();
    let git_repo = test_data.git_repo;
    let commit1 = empty_git_commit(&git_repo, "refs/heads/main", &[]);
    git_repo.set_head("refs/heads/main").unwrap();
    let mut tx = test_data.repo.start_transaction(&test_data.settings);
    let mut_repo = tx.repo_mut();
    git::import_head(mut_repo).unwrap();
    git::import_refs(mut_repo, &git_settings).unwrap();
    mut_repo.rebase_descendants(&test_data.settings).unwrap();
    assert!(git::export_refs(mut_repo).unwrap().is_empty());

    let new_commit = create_random_commit(mut_repo, &test_data.settings)
        .set_parents(vec![jj_id(&commit1)])
        .write()
        .unwrap();
    mut_repo.set_local_bookmark_target("main", RefTarget::normal(new_commit.id().clone()));
    assert!(git::export_refs(mut_repo).unwrap().is_empty());
    assert_eq!(
        mut_repo.get_git_ref("refs/heads/main"),
        RefTarget::normal(new_commit.id().clone())
    );
    assert_eq!(
        git_repo
            .find_reference("refs/heads/main")
            .unwrap()
            .peel_to_commit()
            .unwrap()
            .id(),
        git_id(&new_commit)
    );
    assert!(git_repo.head_detached().unwrap());
}

#[test_case(false; "without moved placeholder ref")]
#[test_case(true; "with moved placeholder ref")]
fn test_export_refs_unborn_git_bookmark(move_placeholder_ref: bool) {
    // Can export to an empty Git repo (we can handle Git's "unborn bookmark" state)
    let test_data = GitRepoData::create();
    let git_settings = GitSettings::default();
    let git_repo = test_data.git_repo;
    git_repo.set_head("refs/heads/main").unwrap();
    let mut tx = test_data.repo.start_transaction(&test_data.settings);
    let mut_repo = tx.repo_mut();
    git::import_head(mut_repo).unwrap();
    git::import_refs(mut_repo, &git_settings).unwrap();
    mut_repo.rebase_descendants(&test_data.settings).unwrap();
    assert!(git::export_refs(mut_repo).unwrap().is_empty());
    assert!(git_repo.head().is_err(), "HEAD is unborn");

    let new_commit = write_random_commit(mut_repo, &test_data.settings);
    mut_repo.set_local_bookmark_target("main", RefTarget::normal(new_commit.id().clone()));
    if move_placeholder_ref {
        git_repo
            .reference("refs/jj/root", git_id(&new_commit), false, "")
            .unwrap();
    }
    assert!(git::export_refs(mut_repo).unwrap().is_empty());
    assert_eq!(
        mut_repo.get_git_ref("refs/heads/main"),
        RefTarget::normal(new_commit.id().clone())
    );
    assert_eq!(
        git_repo
            .find_reference("refs/heads/main")
            .unwrap()
            .peel_to_commit()
            .unwrap()
            .id(),
        git_id(&new_commit)
    );
    // HEAD should no longer point to refs/heads/main
    assert!(git_repo.head().is_err(), "HEAD is unborn");
    // The placeholder ref should be deleted if any
    assert!(git_repo.find_reference("refs/jj/root").is_err());
}

#[test]
fn test_export_import_sequence() {
    // Import a bookmark pointing to A, modify it in jj to point to B, export it,
    // modify it in git to point to C, then import it again. There should be no
    // conflict.
    let test_data = GitRepoData::create();
    let git_settings = GitSettings::default();
    let git_repo = test_data.git_repo;
    let mut tx = test_data.repo.start_transaction(&test_data.settings);
    let mut_repo = tx.repo_mut();
    let commit_a = write_random_commit(mut_repo, &test_data.settings);
    let commit_b = write_random_commit(mut_repo, &test_data.settings);
    let commit_c = write_random_commit(mut_repo, &test_data.settings);

    // Import the bookmark pointing to A
    git_repo
        .reference("refs/heads/main", git_id(&commit_a), true, "test")
        .unwrap();
    git::import_refs(mut_repo, &git_settings).unwrap();
    assert_eq!(
        mut_repo.get_git_ref("refs/heads/main"),
        RefTarget::normal(commit_a.id().clone())
    );

    // Modify the bookmark in jj to point to B
    mut_repo.set_local_bookmark_target("main", RefTarget::normal(commit_b.id().clone()));

    // Export the bookmark to git
    assert!(git::export_refs(mut_repo).unwrap().is_empty());
    assert_eq!(
        mut_repo.get_git_ref("refs/heads/main"),
        RefTarget::normal(commit_b.id().clone())
    );

    // Modify the bookmark in git to point to C
    git_repo
        .reference("refs/heads/main", git_id(&commit_c), true, "test")
        .unwrap();

    // Import from git
    git::import_refs(mut_repo, &git_settings).unwrap();
    assert_eq!(
        mut_repo.get_git_ref("refs/heads/main"),
        RefTarget::normal(commit_c.id().clone())
    );
    assert_eq!(
        mut_repo.view().get_local_bookmark("main"),
        &RefTarget::normal(commit_c.id().clone())
    );
}

#[test]
fn test_import_export_non_tracking_bookmark() {
    // Import a remote tracking bookmark and export it. We should not create a git
    // bookmark.
    let test_data = GitRepoData::create();
    let mut git_settings = GitSettings {
        auto_local_bookmark: false,
        ..Default::default()
    };
    let git_repo = test_data.git_repo;
    let commit_main_t0 = empty_git_commit(&git_repo, "refs/remotes/origin/main", &[]);

    let mut tx = test_data.repo.start_transaction(&test_data.settings);
    let mut_repo = tx.repo_mut();

    git::import_refs(mut_repo, &git_settings).unwrap();

    assert!(mut_repo.view().get_local_bookmark("main").is_absent());
    assert_eq!(
        mut_repo.view().get_remote_bookmark("main", "origin"),
        &RemoteRef {
            target: RefTarget::normal(jj_id(&commit_main_t0)),
            state: RemoteRefState::New,
        },
    );
    assert_eq!(
        mut_repo.get_git_ref("refs/remotes/origin/main"),
        RefTarget::normal(jj_id(&commit_main_t0))
    );

    // Export the bookmark to git
    assert!(git::export_refs(mut_repo).unwrap().is_empty());
    assert_eq!(mut_repo.get_git_ref("refs/heads/main"), RefTarget::absent());

    // Reimport with auto-local-bookmark on. Local bookmark shouldn't be created for
    // the known bookmark "main".
    let commit_main_t1 =
        empty_git_commit(&git_repo, "refs/remotes/origin/main", &[&commit_main_t0]);
    let commit_feat_t1 = empty_git_commit(&git_repo, "refs/remotes/origin/feat", &[]);
    git_settings.auto_local_bookmark = true;
    git::import_refs(mut_repo, &git_settings).unwrap();
    assert!(mut_repo.view().get_local_bookmark("main").is_absent());
    assert_eq!(
        mut_repo.view().get_local_bookmark("feat"),
        &RefTarget::normal(jj_id(&commit_feat_t1))
    );
    assert_eq!(
        mut_repo.view().get_remote_bookmark("main", "origin"),
        &RemoteRef {
            target: RefTarget::normal(jj_id(&commit_main_t1)),
            state: RemoteRefState::New,
        },
    );
    assert_eq!(
        mut_repo.view().get_remote_bookmark("feat", "origin"),
        &RemoteRef {
            target: RefTarget::normal(jj_id(&commit_feat_t1)),
            state: RemoteRefState::Tracking,
        },
    );

    // Reimport with auto-local-bookmark off. Tracking bookmark should be imported.
    let commit_main_t2 =
        empty_git_commit(&git_repo, "refs/remotes/origin/main", &[&commit_main_t1]);
    let commit_feat_t2 =
        empty_git_commit(&git_repo, "refs/remotes/origin/feat", &[&commit_feat_t1]);
    git_settings.auto_local_bookmark = false;
    git::import_refs(mut_repo, &git_settings).unwrap();
    assert!(mut_repo.view().get_local_bookmark("main").is_absent());
    assert_eq!(
        mut_repo.view().get_local_bookmark("feat"),
        &RefTarget::normal(jj_id(&commit_feat_t2))
    );
    assert_eq!(
        mut_repo.view().get_remote_bookmark("main", "origin"),
        &RemoteRef {
            target: RefTarget::normal(jj_id(&commit_main_t2)),
            state: RemoteRefState::New,
        },
    );
    assert_eq!(
        mut_repo.view().get_remote_bookmark("feat", "origin"),
        &RemoteRef {
            target: RefTarget::normal(jj_id(&commit_feat_t2)),
            state: RemoteRefState::Tracking,
        },
    );
}

#[test]
fn test_export_conflicts() {
    // We skip export of conflicted bookmarks
    let test_data = GitRepoData::create();
    let git_repo = test_data.git_repo;
    let mut tx = test_data.repo.start_transaction(&test_data.settings);
    let mut_repo = tx.repo_mut();
    let commit_a = write_random_commit(mut_repo, &test_data.settings);
    let commit_b = write_random_commit(mut_repo, &test_data.settings);
    let commit_c = write_random_commit(mut_repo, &test_data.settings);
    mut_repo.set_local_bookmark_target("main", RefTarget::normal(commit_a.id().clone()));
    mut_repo.set_local_bookmark_target("feature", RefTarget::normal(commit_a.id().clone()));
    assert!(git::export_refs(mut_repo).unwrap().is_empty());

    // Create a conflict and export. It should not be exported, but other changes
    // should be.
    mut_repo.set_local_bookmark_target("main", RefTarget::normal(commit_b.id().clone()));
    mut_repo.set_local_bookmark_target(
        "feature",
        RefTarget::from_legacy_form(
            [commit_a.id().clone()],
            [commit_b.id().clone(), commit_c.id().clone()],
        ),
    );
    assert!(git::export_refs(mut_repo).unwrap().is_empty());
    assert_eq!(
        git_repo
            .find_reference("refs/heads/feature")
            .unwrap()
            .target()
            .unwrap(),
        git_id(&commit_a)
    );
    assert_eq!(
        git_repo
            .find_reference("refs/heads/main")
            .unwrap()
            .target()
            .unwrap(),
        git_id(&commit_b)
    );

    // Conflicted bookmarks shouldn't be copied to the "git" remote
    assert_eq!(
        mut_repo.get_remote_bookmark("feature", "git"),
        RemoteRef {
            target: RefTarget::normal(commit_a.id().clone()),
            state: RemoteRefState::Tracking,
        },
    );
    assert_eq!(
        mut_repo.get_remote_bookmark("main", "git"),
        RemoteRef {
            target: RefTarget::normal(commit_b.id().clone()),
            state: RemoteRefState::Tracking,
        },
    );
}

#[test]
fn test_export_bookmark_on_root_commit() {
    // We skip export of bookmarks pointing to the root commit
    let test_data = GitRepoData::create();
    let mut tx = test_data.repo.start_transaction(&test_data.settings);
    let mut_repo = tx.repo_mut();
    mut_repo.set_local_bookmark_target(
        "on_root",
        RefTarget::normal(mut_repo.store().root_commit_id().clone()),
    );
    let failed = git::export_refs(mut_repo).unwrap();
    assert_eq!(failed.len(), 1);
    assert_eq!(failed[0].name, RefName::LocalBranch("on_root".to_string()));
    assert_matches!(failed[0].reason, FailedRefExportReason::OnRootCommit);
}

#[test]
fn test_export_partial_failure() {
    // Check that we skip bookmarks that fail to export
    let test_data = GitRepoData::create();
    let git_repo = test_data.git_repo;
    let mut tx = test_data.repo.start_transaction(&test_data.settings);
    let mut_repo = tx.repo_mut();
    let commit_a = write_random_commit(mut_repo, &test_data.settings);
    let target = RefTarget::normal(commit_a.id().clone());
    // Empty string is disallowed by Git
    mut_repo.set_local_bookmark_target("", target.clone());
    // Branch named HEAD is disallowed by Git CLI
    mut_repo.set_local_bookmark_target("HEAD", target.clone());
    mut_repo.set_local_bookmark_target("main", target.clone());
    // `main/sub` will conflict with `main` in Git, at least when using loose ref
    // storage
    mut_repo.set_local_bookmark_target("main/sub", target.clone());
    let failed = git::export_refs(mut_repo).unwrap();
    assert_eq!(failed.len(), 3);
    assert_eq!(failed[0].name, RefName::LocalBranch("".to_string()));
    assert_matches!(failed[0].reason, FailedRefExportReason::InvalidGitName);
    assert_eq!(failed[1].name, RefName::LocalBranch("HEAD".to_string()));
    assert_matches!(failed[1].reason, FailedRefExportReason::InvalidGitName);
    assert_eq!(failed[2].name, RefName::LocalBranch("main/sub".to_string()));
    assert_matches!(failed[2].reason, FailedRefExportReason::FailedToSet(_));

    // The `main` bookmark should have succeeded but the other should have failed
    assert!(git_repo.find_reference("refs/heads/").is_err());
    assert!(git_repo.find_reference("refs/heads/HEAD").is_err());
    assert_eq!(
        git_repo
            .find_reference("refs/heads/main")
            .unwrap()
            .target()
            .unwrap(),
        git_id(&commit_a)
    );
    assert!(git_repo.find_reference("refs/heads/main/sub").is_err());

    // Failed bookmarks shouldn't be copied to the "git" remote
    assert!(mut_repo.get_remote_bookmark("", "git").is_absent());
    assert!(mut_repo.get_remote_bookmark("HEAD", "git").is_absent());
    assert_eq!(
        mut_repo.get_remote_bookmark("main", "git"),
        RemoteRef {
            target: target.clone(),
            state: RemoteRefState::Tracking,
        },
    );
    assert!(mut_repo.get_remote_bookmark("main/sub", "git").is_absent());

    // Now remove the `main` bookmark and make sure that the `main/sub` gets
    // exported even though it didn't change
    mut_repo.set_local_bookmark_target("main", RefTarget::absent());
    let failed = git::export_refs(mut_repo).unwrap();
    assert_eq!(failed.len(), 2);
    assert_eq!(failed[0].name, RefName::LocalBranch("".to_string()));
    assert_matches!(failed[0].reason, FailedRefExportReason::InvalidGitName);
    assert_eq!(failed[1].name, RefName::LocalBranch("HEAD".to_string()));
    assert_matches!(failed[1].reason, FailedRefExportReason::InvalidGitName);
    assert!(git_repo.find_reference("refs/heads/").is_err());
    assert!(git_repo.find_reference("refs/heads/HEAD").is_err());
    assert!(git_repo.find_reference("refs/heads/main").is_err());
    assert_eq!(
        git_repo
            .find_reference("refs/heads/main/sub")
            .unwrap()
            .target()
            .unwrap(),
        git_id(&commit_a)
    );

    // Failed bookmarks shouldn't be copied to the "git" remote
    assert!(mut_repo.get_remote_bookmark("", "git").is_absent());
    assert!(mut_repo.get_remote_bookmark("HEAD", "git").is_absent());
    assert!(mut_repo.get_remote_bookmark("main", "git").is_absent());
    assert_eq!(
        mut_repo.get_remote_bookmark("main/sub", "git"),
        RemoteRef {
            target: target.clone(),
            state: RemoteRefState::Tracking,
        },
    );
}

#[test]
fn test_export_reexport_transitions() {
    // Test exporting after making changes on the jj side, or the git side, or both
    let test_data = GitRepoData::create();
    let git_repo = test_data.git_repo;
    let mut tx = test_data.repo.start_transaction(&test_data.settings);
    let mut_repo = tx.repo_mut();
    let commit_a = write_random_commit(mut_repo, &test_data.settings);
    let commit_b = write_random_commit(mut_repo, &test_data.settings);
    let commit_c = write_random_commit(mut_repo, &test_data.settings);
    // Create a few bookmarks whose names indicate how they change in jj in git. The
    // first letter represents the bookmark's target in the last export. The second
    // letter represents the bookmark's target in jj. The third letter represents
    // the bookmark's target in git. "X" means that the bookmark doesn't exist.
    // "A", "B", or "C" means that the bookmark points to that commit.
    //
    // AAB: Branch modified in git
    // AAX: Branch deleted in git
    // ABA: Branch modified in jj
    // ABB: Branch modified in both jj and git, pointing to same target
    // ABC: Branch modified in both jj and git, pointing to different targets
    // ABX: Branch modified in jj, deleted in git
    // AXA: Branch deleted in jj
    // AXB: Branch deleted in jj, modified in git
    // AXX: Branch deleted in both jj and git
    // XAA: Branch added in both jj and git, pointing to same target
    // XAB: Branch added in both jj and git, pointing to different targets
    // XAX: Branch added in jj
    // XXA: Branch added in git

    // Create initial state and export it
    for bookmark in [
        "AAB", "AAX", "ABA", "ABB", "ABC", "ABX", "AXA", "AXB", "AXX",
    ] {
        mut_repo.set_local_bookmark_target(bookmark, RefTarget::normal(commit_a.id().clone()));
    }
    assert!(git::export_refs(mut_repo).unwrap().is_empty());

    // Make changes on the jj side
    for bookmark in ["AXA", "AXB", "AXX"] {
        mut_repo.set_local_bookmark_target(bookmark, RefTarget::absent());
    }
    for bookmark in ["XAA", "XAB", "XAX"] {
        mut_repo.set_local_bookmark_target(bookmark, RefTarget::normal(commit_a.id().clone()));
    }
    for bookmark in ["ABA", "ABB", "ABC", "ABX"] {
        mut_repo.set_local_bookmark_target(bookmark, RefTarget::normal(commit_b.id().clone()));
    }

    // Make changes on the git side
    for bookmark in ["AAX", "ABX", "AXX"] {
        git_repo
            .find_reference(&format!("refs/heads/{bookmark}"))
            .unwrap()
            .delete()
            .unwrap();
    }
    for bookmark in ["XAA", "XXA"] {
        git_repo
            .reference(
                &format!("refs/heads/{bookmark}"),
                git_id(&commit_a),
                true,
                "",
            )
            .unwrap();
    }
    for bookmark in ["AAB", "ABB", "AXB", "XAB"] {
        git_repo
            .reference(
                &format!("refs/heads/{bookmark}"),
                git_id(&commit_b),
                true,
                "",
            )
            .unwrap();
    }
    let bookmark = "ABC";
    git_repo
        .reference(
            &format!("refs/heads/{bookmark}"),
            git_id(&commit_c),
            true,
            "",
        )
        .unwrap();

    // TODO: The bookmarks that we made conflicting changes to should have failed to
    // export. They should have been unchanged in git and in
    // mut_repo.view().git_refs().
    assert_eq!(
        git::export_refs(mut_repo)
            .unwrap()
            .into_iter()
            .map(|failed| failed.name)
            .collect_vec(),
        vec!["ABC", "ABX", "AXB", "XAB"]
            .into_iter()
            .map(|s| RefName::LocalBranch(s.to_string()))
            .collect_vec()
    );
    for bookmark in ["AAX", "ABX", "AXA", "AXX"] {
        assert!(
            git_repo
                .find_reference(&format!("refs/heads/{bookmark}"))
                .is_err(),
            "{bookmark} should not exist"
        );
    }
    for bookmark in ["XAA", "XAX", "XXA"] {
        assert_eq!(
            git_repo
                .find_reference(&format!("refs/heads/{bookmark}"))
                .unwrap()
                .target(),
            Some(git_id(&commit_a)),
            "{bookmark} should point to commit A"
        );
    }
    for bookmark in ["AAB", "ABA", "AAB", "ABB", "AXB", "XAB"] {
        assert_eq!(
            git_repo
                .find_reference(&format!("refs/heads/{bookmark}"))
                .unwrap()
                .target(),
            Some(git_id(&commit_b)),
            "{bookmark} should point to commit B"
        );
    }
    let bookmark = "ABC";
    assert_eq!(
        git_repo
            .find_reference(&format!("refs/heads/{bookmark}"))
            .unwrap()
            .target(),
        Some(git_id(&commit_c)),
        "{bookmark} should point to commit C"
    );
    assert_eq!(
        *mut_repo.view().git_refs(),
        btreemap! {
            "refs/heads/AAX".to_string() => RefTarget::normal(commit_a.id().clone()),
            "refs/heads/AAB".to_string() => RefTarget::normal(commit_a.id().clone()),
            "refs/heads/ABA".to_string() => RefTarget::normal(commit_b.id().clone()),
            "refs/heads/ABB".to_string() => RefTarget::normal(commit_b.id().clone()),
            "refs/heads/ABC".to_string() => RefTarget::normal(commit_a.id().clone()),
            "refs/heads/ABX".to_string() => RefTarget::normal(commit_a.id().clone()),
            "refs/heads/AXB".to_string() => RefTarget::normal(commit_a.id().clone()),
            "refs/heads/XAA".to_string() => RefTarget::normal(commit_a.id().clone()),
            "refs/heads/XAX".to_string() => RefTarget::normal(commit_a.id().clone()),
        }
    );
}

#[test]
fn test_export_undo_reexport() {
    let test_data = GitRepoData::create();
    let git_repo = test_data.git_repo;
    let mut tx = test_data.repo.start_transaction(&test_data.settings);
    let mut_repo = tx.repo_mut();

    // Initial export
    let commit_a = write_random_commit(mut_repo, &test_data.settings);
    let target_a = RefTarget::normal(commit_a.id().clone());
    mut_repo.set_local_bookmark_target("main", target_a.clone());
    assert!(git::export_refs(mut_repo).unwrap().is_empty());
    assert_eq!(
        git_repo.find_reference("refs/heads/main").unwrap().target(),
        Some(git_id(&commit_a))
    );
    assert_eq!(mut_repo.get_git_ref("refs/heads/main"), target_a);
    assert_eq!(
        mut_repo.get_remote_bookmark("main", "git"),
        RemoteRef {
            target: target_a.clone(),
            state: RemoteRefState::Tracking,
        },
    );

    // Undo remote changes only
    mut_repo.set_remote_bookmark("main", "git", RemoteRef::absent());

    // Reexport should update the Git-tracking bookmark
    assert!(git::export_refs(mut_repo).unwrap().is_empty());
    assert_eq!(
        git_repo.find_reference("refs/heads/main").unwrap().target(),
        Some(git_id(&commit_a))
    );
    assert_eq!(mut_repo.get_git_ref("refs/heads/main"), target_a);
    assert_eq!(
        mut_repo.get_remote_bookmark("main", "git"),
        RemoteRef {
            target: target_a.clone(),
            state: RemoteRefState::Tracking,
        },
    );
}

#[test]
fn test_reset_head_to_root() {
    // Create colocated workspace
    let settings = testutils::user_settings();
    let temp_dir = testutils::new_temp_dir();
    let workspace_root = temp_dir.path().join("repo");
    let git_repo = git2::Repository::init(&workspace_root).unwrap();
    let (_workspace, repo) =
        Workspace::init_external_git(&settings, &workspace_root, &workspace_root.join(".git"))
            .unwrap();

    let mut tx = repo.start_transaction(&settings);
    let mut_repo = tx.repo_mut();

    let root_commit_id = repo.store().root_commit_id();
    let tree_id = repo.store().empty_merged_tree_id();
    let commit1 = mut_repo
        .new_commit(&settings, vec![root_commit_id.clone()], tree_id.clone())
        .write()
        .unwrap();
    let commit2 = mut_repo
        .new_commit(&settings, vec![commit1.id().clone()], tree_id.clone())
        .write()
        .unwrap();

    // Set Git HEAD to commit2's parent (i.e. commit1)
    git::reset_head(tx.repo_mut(), &git_repo, &commit2).unwrap();
    assert!(git_repo.head().is_ok());
    assert_eq!(
        tx.repo_mut().git_head(),
        RefTarget::normal(commit1.id().clone())
    );

    // Set Git HEAD back to root
    git::reset_head(tx.repo_mut(), &git_repo, &commit1).unwrap();
    assert!(git_repo.head().is_err());
    assert!(tx.repo_mut().git_head().is_absent());

    // Move placeholder ref as if new commit were created by git
    git_repo
        .reference("refs/jj/root", git_id(&commit1), false, "")
        .unwrap();
    git::reset_head(tx.repo_mut(), &git_repo, &commit2).unwrap();
    assert!(git_repo.head().is_ok());
    assert_eq!(
        tx.repo_mut().git_head(),
        RefTarget::normal(commit1.id().clone())
    );
    assert!(git_repo.find_reference("refs/jj/root").is_ok());

    // Set Git HEAD back to root
    git::reset_head(tx.repo_mut(), &git_repo, &commit1).unwrap();
    assert!(git_repo.head().is_err());
    assert!(tx.repo_mut().git_head().is_absent());
    // The placeholder ref should be deleted
    assert!(git_repo.find_reference("refs/jj/root").is_err());
}

#[test]
fn test_reset_head_with_index() {
    // Create colocated workspace
    let settings = testutils::user_settings();
    let temp_dir = testutils::new_temp_dir();
    let workspace_root = temp_dir.path().join("repo");
    let git_repo = git2::Repository::init(&workspace_root).unwrap();
    let (_workspace, repo) =
        Workspace::init_external_git(&settings, &workspace_root, &workspace_root.join(".git"))
            .unwrap();

    let mut tx = repo.start_transaction(&settings);
    let mut_repo = tx.repo_mut();

    let root_commit_id = repo.store().root_commit_id();
    let tree_id = repo.store().empty_merged_tree_id();
    let commit1 = mut_repo
        .new_commit(&settings, vec![root_commit_id.clone()], tree_id.clone())
        .write()
        .unwrap();
    let commit2 = mut_repo
        .new_commit(&settings, vec![commit1.id().clone()], tree_id.clone())
        .write()
        .unwrap();

    // Set Git HEAD to commit2's parent (i.e. commit1)
    git::reset_head(tx.repo_mut(), &git_repo, &commit2).unwrap();
    assert!(git_repo.index().unwrap().is_empty());

    // Add "staged changes" to the Git index
    let file_path = RepoPath::from_internal_string("file.txt");
    testutils::write_working_copy_file(&workspace_root, file_path, "i am a file\n");
    git_repo
        .index()
        .unwrap()
        .add_path(&file_path.to_fs_path_unchecked(Path::new("")))
        .unwrap();
    assert!(!git_repo.index().unwrap().is_empty());

    // Reset head to and the Git index
    git::reset_head(tx.repo_mut(), &git_repo, &commit2).unwrap();
    assert!(git_repo.index().unwrap().is_empty());
}

#[test]
fn test_init() {
    let settings = testutils::user_settings();
    let temp_dir = testutils::new_temp_dir();
    let git_repo_dir = temp_dir.path().join("git");
    let jj_repo_dir = temp_dir.path().join("jj");
    let git_repo = git2::Repository::init_bare(git_repo_dir).unwrap();
    let initial_git_commit = empty_git_commit(&git_repo, "refs/heads/main", &[]);
    std::fs::create_dir(&jj_repo_dir).unwrap();
    let repo = &ReadonlyRepo::init(
        &settings,
        &jj_repo_dir,
        &|settings, store_path| {
            Ok(Box::new(GitBackend::init_external(
                settings,
                store_path,
                git_repo.path(),
            )?))
        },
        Signer::from_settings(&settings).unwrap(),
        ReadonlyRepo::default_op_store_initializer(),
        ReadonlyRepo::default_op_heads_store_initializer(),
        ReadonlyRepo::default_index_store_initializer(),
        ReadonlyRepo::default_submodule_store_initializer(),
    )
    .unwrap();
    // The refs were *not* imported -- it's the caller's responsibility to import
    // any refs they care about.
    assert!(!repo.view().heads().contains(&jj_id(&initial_git_commit)));
}

#[test]
fn test_fetch_empty_repo() {
    let test_data = GitRepoData::create();
    let git_settings = GitSettings::default();

    let mut tx = test_data.repo.start_transaction(&test_data.settings);
    let stats = git_fetch(
        tx.repo_mut(),
        &test_data.git_repo,
        "origin",
        &[StringPattern::everything()],
        git::RemoteCallbacks::default(),
        &git_settings,
        None,
    )
    .unwrap();
    // No default bookmark and no refs
    assert_eq!(stats.default_branch, None);
    assert!(stats.import_stats.abandoned_commits.is_empty());
    assert_eq!(*tx.repo_mut().view().git_refs(), btreemap! {});
    assert_eq!(tx.repo_mut().view().bookmarks().count(), 0);
}

#[test]
fn test_fetch_initial_commit_head_is_not_set() {
    let test_data = GitRepoData::create();
    let git_settings = GitSettings {
        auto_local_bookmark: true,
        ..Default::default()
    };
    let initial_git_commit = empty_git_commit(&test_data.origin_repo, "refs/heads/main", &[]);

    let mut tx = test_data.repo.start_transaction(&test_data.settings);
    let stats = git_fetch(
        tx.repo_mut(),
        &test_data.git_repo,
        "origin",
        &[StringPattern::everything()],
        git::RemoteCallbacks::default(),
        &git_settings,
        None,
    )
    .unwrap();
    // No default bookmark because the origin repo's HEAD wasn't set
    assert_eq!(stats.default_branch, None);
    assert!(stats.import_stats.abandoned_commits.is_empty());
    let repo = tx.commit("test").unwrap();
    // The initial commit is visible after git_fetch().
    let view = repo.view();
    assert!(view.heads().contains(&jj_id(&initial_git_commit)));
    let initial_commit_target = RefTarget::normal(jj_id(&initial_git_commit));
    let initial_commit_remote_ref = RemoteRef {
        target: initial_commit_target.clone(),
        state: RemoteRefState::Tracking,
    };
    assert_eq!(
        *view.git_refs(),
        btreemap! {
            "refs/remotes/origin/main".to_string() => initial_commit_target.clone(),
        }
    );
    assert_eq!(
        view.bookmarks().collect::<BTreeMap<_, _>>(),
        btreemap! {
            "main" => BookmarkTarget {
                local_target: &initial_commit_target,
                remote_refs: vec![
                    ("origin", &initial_commit_remote_ref),
                ],
            },
        }
    );
}

#[test]
fn test_fetch_initial_commit_head_is_set() {
    let test_data = GitRepoData::create();
    let git_settings = GitSettings {
        auto_local_bookmark: true,
        ..Default::default()
    };
    let initial_git_commit = empty_git_commit(&test_data.origin_repo, "refs/heads/main", &[]);
    test_data.origin_repo.set_head("refs/heads/main").unwrap();
    let new_git_commit = empty_git_commit(
        &test_data.origin_repo,
        "refs/heads/main",
        &[&initial_git_commit],
    );
    test_data
        .origin_repo
        .reference("refs/tags/v1.0", new_git_commit.id(), false, "")
        .unwrap();

    let mut tx = test_data.repo.start_transaction(&test_data.settings);
    let stats = git_fetch(
        tx.repo_mut(),
        &test_data.git_repo,
        "origin",
        &[StringPattern::everything()],
        git::RemoteCallbacks::default(),
        &git_settings,
        None,
    )
    .unwrap();

    assert_eq!(stats.default_branch, Some("main".to_string()));
    assert!(stats.import_stats.abandoned_commits.is_empty());
}

#[test]
fn test_fetch_success() {
    let mut test_data = GitRepoData::create();
    let git_settings = GitSettings {
        auto_local_bookmark: true,
        ..Default::default()
    };
    let initial_git_commit = empty_git_commit(&test_data.origin_repo, "refs/heads/main", &[]);

    let mut tx = test_data.repo.start_transaction(&test_data.settings);
    git_fetch(
        tx.repo_mut(),
        &test_data.git_repo,
        "origin",
        &[StringPattern::everything()],
        git::RemoteCallbacks::default(),
        &git_settings,
        None,
    )
    .unwrap();
    test_data.repo = tx.commit("test").unwrap();

    test_data.origin_repo.set_head("refs/heads/main").unwrap();
    let new_git_commit = empty_git_commit(
        &test_data.origin_repo,
        "refs/heads/main",
        &[&initial_git_commit],
    );
    test_data
        .origin_repo
        .reference("refs/tags/v1.0", new_git_commit.id(), false, "")
        .unwrap();

    let mut tx = test_data.repo.start_transaction(&test_data.settings);
    let stats = git_fetch(
        tx.repo_mut(),
        &test_data.git_repo,
        "origin",
        &[StringPattern::everything()],
        git::RemoteCallbacks::default(),
        &git_settings,
        None,
    )
    .unwrap();
    // The default bookmark is "main"
    assert_eq!(stats.default_branch, Some("main".to_string()));
    assert!(stats.import_stats.abandoned_commits.is_empty());
    let repo = tx.commit("test").unwrap();
    // The new commit is visible after we fetch again
    let view = repo.view();
    assert!(view.heads().contains(&jj_id(&new_git_commit)));
    let new_commit_target = RefTarget::normal(jj_id(&new_git_commit));
    let new_commit_remote_ref = RemoteRef {
        target: new_commit_target.clone(),
        state: RemoteRefState::Tracking,
    };
    assert_eq!(
        *view.git_refs(),
        btreemap! {
            "refs/remotes/origin/main".to_string() => new_commit_target.clone(),
            "refs/tags/v1.0".to_string() => new_commit_target.clone(),
        }
    );
    assert_eq!(
        view.bookmarks().collect::<BTreeMap<_, _>>(),
        btreemap! {
            "main" => BookmarkTarget {
                local_target: &new_commit_target,
                remote_refs: vec![
                    ("origin", &new_commit_remote_ref),
                ],
            },
        }
    );
    assert_eq!(
        *view.tags(),
        btreemap! {
            "v1.0".to_string() => new_commit_target.clone(),
        }
    );
}

#[test]
fn test_fetch_prune_deleted_ref() {
    let test_data = GitRepoData::create();
    let git_settings = GitSettings {
        auto_local_bookmark: true,
        ..Default::default()
    };
    let commit = empty_git_commit(&test_data.origin_repo, "refs/heads/main", &[]);

    let mut tx = test_data.repo.start_transaction(&test_data.settings);
    git_fetch(
        tx.repo_mut(),
        &test_data.git_repo,
        "origin",
        &[StringPattern::everything()],
        git::RemoteCallbacks::default(),
        &git_settings,
        None,
    )
    .unwrap();
    // Test the setup
    assert!(tx.repo_mut().get_local_bookmark("main").is_present());
    assert!(tx
        .repo_mut()
        .get_remote_bookmark("main", "origin")
        .is_present());

    test_data
        .origin_repo
        .find_reference("refs/heads/main")
        .unwrap()
        .delete()
        .unwrap();
    // After re-fetching, the bookmark should be deleted
    let stats = git_fetch(
        tx.repo_mut(),
        &test_data.git_repo,
        "origin",
        &[StringPattern::everything()],
        git::RemoteCallbacks::default(),
        &git_settings,
        None,
    )
    .unwrap();
    assert_eq!(stats.import_stats.abandoned_commits, vec![jj_id(&commit)]);
    assert!(tx.repo_mut().get_local_bookmark("main").is_absent());
    assert!(tx
        .repo_mut()
        .get_remote_bookmark("main", "origin")
        .is_absent());
}

#[test]
fn test_fetch_no_default_branch() {
    let test_data = GitRepoData::create();
    let git_settings = GitSettings {
        auto_local_bookmark: true,
        ..Default::default()
    };
    let initial_git_commit = empty_git_commit(&test_data.origin_repo, "refs/heads/main", &[]);

    let mut tx = test_data.repo.start_transaction(&test_data.settings);
    git_fetch(
        tx.repo_mut(),
        &test_data.git_repo,
        "origin",
        &[StringPattern::everything()],
        git::RemoteCallbacks::default(),
        &git_settings,
        None,
    )
    .unwrap();

    empty_git_commit(
        &test_data.origin_repo,
        "refs/heads/main",
        &[&initial_git_commit],
    );
    // It's actually not enough to have a detached HEAD, it also needs to point to a
    // commit without a bookmark (that's possibly a bug in Git *and* libgit2), so
    // we point it to initial_git_commit.
    test_data
        .origin_repo
        .set_head_detached(initial_git_commit.id())
        .unwrap();

    let stats = git_fetch(
        tx.repo_mut(),
        &test_data.git_repo,
        "origin",
        &[StringPattern::everything()],
        git::RemoteCallbacks::default(),
        &git_settings,
        None,
    )
    .unwrap();
    // There is no default bookmark
    assert_eq!(stats.default_branch, None);
}

#[test]
fn test_fetch_empty_refspecs() {
    let test_data = GitRepoData::create();
    let git_settings = GitSettings::default();
    empty_git_commit(&test_data.origin_repo, "refs/heads/main", &[]);

    // Base refspecs shouldn't be respected
    let mut tx = test_data.repo.start_transaction(&test_data.settings);
    git_fetch(
        tx.repo_mut(),
        &test_data.git_repo,
        "origin",
        &[],
        git::RemoteCallbacks::default(),
        &git_settings,
        None,
    )
    .unwrap();
    assert!(tx
        .repo_mut()
        .get_remote_bookmark("main", "origin")
        .is_absent());
    // No remote refs should have been fetched
    git::import_refs(tx.repo_mut(), &git_settings).unwrap();
    assert!(tx
        .repo_mut()
        .get_remote_bookmark("main", "origin")
        .is_absent());
}

#[test]
fn test_fetch_no_such_remote() {
    let test_data = GitRepoData::create();
    let git_settings = GitSettings::default();
    let mut tx = test_data.repo.start_transaction(&test_data.settings);
    let result = git_fetch(
        tx.repo_mut(),
        &test_data.git_repo,
        "invalid-remote",
        &[StringPattern::everything()],
        git::RemoteCallbacks::default(),
        &git_settings,
        None,
    );
    assert!(matches!(result, Err(GitFetchError::NoSuchRemote(_))));
}

struct PushTestSetup {
    source_repo_dir: PathBuf,
    jj_repo: Arc<ReadonlyRepo>,
    main_commit: Commit,
    child_of_main_commit: Commit,
    parent_of_main_commit: Commit,
    sideways_commit: Commit,
}

/// Set up a situation where `main` is at `main_commit`, the child of
/// `parent_of_main_commit`, both in the source repo and in jj's clone of the
/// repo. In jj's clone, there are also two more commits, `child_of_main_commit`
/// and `sideways_commit`, arranged as follows:
///
/// o    child_of_main_commit
/// o    main_commit
/// o    parent_of_main_commit
/// | o  sideways_commit
/// |/
/// ~    root
fn set_up_push_repos(settings: &UserSettings, temp_dir: &TempDir) -> PushTestSetup {
    let source_repo_dir = temp_dir.path().join("source");
    let clone_repo_dir = temp_dir.path().join("clone");
    let jj_repo_dir = temp_dir.path().join("jj");
    let source_repo = git2::Repository::init_bare(&source_repo_dir).unwrap();
    let parent_of_initial_git_commit = empty_git_commit(&source_repo, "refs/heads/main", &[]);
    let initial_git_commit = empty_git_commit(
        &source_repo,
        "refs/heads/main",
        &[&parent_of_initial_git_commit],
    );
    let clone_repo =
        git2::Repository::clone(source_repo_dir.to_str().unwrap(), clone_repo_dir).unwrap();
    std::fs::create_dir(&jj_repo_dir).unwrap();
    let jj_repo = ReadonlyRepo::init(
        settings,
        &jj_repo_dir,
        &|settings, store_path| {
            Ok(Box::new(GitBackend::init_external(
                settings,
                store_path,
                clone_repo.path(),
            )?))
        },
        Signer::from_settings(settings).unwrap(),
        ReadonlyRepo::default_op_store_initializer(),
        ReadonlyRepo::default_op_heads_store_initializer(),
        ReadonlyRepo::default_index_store_initializer(),
        ReadonlyRepo::default_submodule_store_initializer(),
    )
    .unwrap();
    get_git_backend(&jj_repo)
        .import_head_commits(&[jj_id(&initial_git_commit)])
        .unwrap();
    let main_commit = jj_repo
        .store()
        .get_commit(&jj_id(&initial_git_commit))
        .unwrap();
    let parent_of_main_commit = jj_repo
        .store()
        .get_commit(&jj_id(&parent_of_initial_git_commit))
        .unwrap();
    let mut tx = jj_repo.start_transaction(settings);
    let sideways_commit = write_random_commit(tx.repo_mut(), settings);
    let child_of_main_commit = create_random_commit(tx.repo_mut(), settings)
        .set_parents(vec![main_commit.id().clone()])
        .write()
        .unwrap();
    tx.repo_mut().set_git_ref_target(
        "refs/remotes/origin/main",
        RefTarget::normal(main_commit.id().clone()),
    );
    tx.repo_mut().set_remote_bookmark(
        "main",
        "origin",
        RemoteRef {
            target: RefTarget::normal(main_commit.id().clone()),
            // Caller expects the main bookmark is tracked. The corresponding local bookmark will
            // be created (or left as deleted) by caller.
            state: RemoteRefState::Tracking,
        },
    );
    let jj_repo = tx.commit("test").unwrap();
    PushTestSetup {
        source_repo_dir,
        jj_repo,
        main_commit,
        child_of_main_commit,
        parent_of_main_commit,
        sideways_commit,
    }
}

#[test]
fn test_push_bookmarks_success() {
    let settings = testutils::user_settings();
    let temp_dir = testutils::new_temp_dir();
    let mut setup = set_up_push_repos(&settings, &temp_dir);
    let clone_repo = get_git_repo(&setup.jj_repo);
    let mut tx = setup.jj_repo.start_transaction(&settings);

    let targets = GitBranchPushTargets {
        branch_updates: vec![(
            "main".to_owned(),
            BookmarkPushUpdate {
                old_target: Some(setup.main_commit.id().clone()),
                new_target: Some(setup.child_of_main_commit.id().clone()),
            },
        )],
    };
    let result = git::push_branches(
        tx.repo_mut(),
        &clone_repo,
        "origin",
        &targets,
        git::RemoteCallbacks::default(),
    );
    assert_eq!(result, Ok(()));

    // Check that the ref got updated in the source repo
    let source_repo = git2::Repository::open(&setup.source_repo_dir).unwrap();
    let new_target = source_repo
        .find_reference("refs/heads/main")
        .unwrap()
        .target();
    let new_oid = git_id(&setup.child_of_main_commit);
    assert_eq!(new_target, Some(new_oid));

    // Check that the ref got updated in the cloned repo. This just tests our
    // assumptions about libgit2 because we want the refs/remotes/origin/main
    // bookmark to be updated.
    let new_target = clone_repo
        .find_reference("refs/remotes/origin/main")
        .unwrap()
        .target();
    assert_eq!(new_target, Some(new_oid));

    // Check that the repo view got updated
    let view = tx.repo_mut().view();
    assert_eq!(
        *view.get_git_ref("refs/remotes/origin/main"),
        RefTarget::normal(setup.child_of_main_commit.id().clone()),
    );
    assert_eq!(
        *view.get_remote_bookmark("main", "origin"),
        RemoteRef {
            target: RefTarget::normal(setup.child_of_main_commit.id().clone()),
            state: RemoteRefState::Tracking,
        },
    );

    // Check that the repo view reflects the changes in the Git repo
    setup.jj_repo = tx.commit("test").unwrap();
    let mut tx = setup.jj_repo.start_transaction(&settings);
    git::import_refs(tx.repo_mut(), &GitSettings::default()).unwrap();
    assert!(!tx.repo_mut().has_changes());
}

#[test]
fn test_push_bookmarks_deletion() {
    let settings = testutils::user_settings();
    let temp_dir = testutils::new_temp_dir();
    let mut setup = set_up_push_repos(&settings, &temp_dir);
    let clone_repo = get_git_repo(&setup.jj_repo);
    let mut tx = setup.jj_repo.start_transaction(&settings);

    let source_repo = git2::Repository::open(&setup.source_repo_dir).unwrap();
    // Test the setup
    assert!(source_repo.find_reference("refs/heads/main").is_ok());

    let targets = GitBranchPushTargets {
        branch_updates: vec![(
            "main".to_owned(),
            BookmarkPushUpdate {
                old_target: Some(setup.main_commit.id().clone()),
                new_target: None,
            },
        )],
    };
    let result = git::push_branches(
        tx.repo_mut(),
        &get_git_repo(&setup.jj_repo),
        "origin",
        &targets,
        git::RemoteCallbacks::default(),
    );
    assert_eq!(result, Ok(()));

    // Check that the ref got deleted in the source repo
    assert!(source_repo.find_reference("refs/heads/main").is_err());

    // Check that the ref got deleted in the cloned repo. This just tests our
    // assumptions about libgit2 because we want the refs/remotes/origin/main
    // bookmark to be deleted.
    assert!(clone_repo
        .find_reference("refs/remotes/origin/main")
        .is_err());

    // Check that the repo view got updated
    let view = tx.repo_mut().view();
    assert!(view.get_git_ref("refs/remotes/origin/main").is_absent());
    assert!(view.get_remote_bookmark("main", "origin").is_absent());

    // Check that the repo view reflects the changes in the Git repo
    setup.jj_repo = tx.commit("test").unwrap();
    let mut tx = setup.jj_repo.start_transaction(&settings);
    git::import_refs(tx.repo_mut(), &GitSettings::default()).unwrap();
    assert!(!tx.repo_mut().has_changes());
}

#[test]
fn test_push_bookmarks_mixed_deletion_and_addition() {
    let settings = testutils::user_settings();
    let temp_dir = testutils::new_temp_dir();
    let mut setup = set_up_push_repos(&settings, &temp_dir);
    let clone_repo = get_git_repo(&setup.jj_repo);
    let mut tx = setup.jj_repo.start_transaction(&settings);

    let targets = GitBranchPushTargets {
        branch_updates: vec![
            (
                "main".to_owned(),
                BookmarkPushUpdate {
                    old_target: Some(setup.main_commit.id().clone()),
                    new_target: None,
                },
            ),
            (
                "topic".to_owned(),
                BookmarkPushUpdate {
                    old_target: None,
                    new_target: Some(setup.child_of_main_commit.id().clone()),
                },
            ),
        ],
    };
    let result = git::push_branches(
        tx.repo_mut(),
        &clone_repo,
        "origin",
        &targets,
        git::RemoteCallbacks::default(),
    );
    assert_eq!(result, Ok(()));

    // Check that the topic ref got updated in the source repo
    let source_repo = git2::Repository::open(&setup.source_repo_dir).unwrap();
    let new_target = source_repo
        .find_reference("refs/heads/topic")
        .unwrap()
        .target();
    assert_eq!(new_target, Some(git_id(&setup.child_of_main_commit)));

    // Check that the main ref got deleted in the source repo
    assert!(source_repo.find_reference("refs/heads/main").is_err());

    // Check that the repo view got updated
    let view = tx.repo_mut().view();
    assert!(view.get_git_ref("refs/remotes/origin/main").is_absent());
    assert!(view.get_remote_bookmark("main", "origin").is_absent());
    assert_eq!(
        *view.get_git_ref("refs/remotes/origin/topic"),
        RefTarget::normal(setup.child_of_main_commit.id().clone()),
    );
    assert_eq!(
        *view.get_remote_bookmark("topic", "origin"),
        RemoteRef {
            target: RefTarget::normal(setup.child_of_main_commit.id().clone()),
            state: RemoteRefState::Tracking,
        },
    );

    // Check that the repo view reflects the changes in the Git repo
    setup.jj_repo = tx.commit("test").unwrap();
    let mut tx = setup.jj_repo.start_transaction(&settings);
    git::import_refs(tx.repo_mut(), &GitSettings::default()).unwrap();
    assert!(!tx.repo_mut().has_changes());
}

#[test]
fn test_push_bookmarks_not_fast_forward() {
    let settings = testutils::user_settings();
    let temp_dir = testutils::new_temp_dir();
    let setup = set_up_push_repos(&settings, &temp_dir);
    let mut tx = setup.jj_repo.start_transaction(&settings);

    let targets = GitBranchPushTargets {
        branch_updates: vec![(
            "main".to_owned(),
            BookmarkPushUpdate {
                old_target: Some(setup.main_commit.id().clone()),
                new_target: Some(setup.sideways_commit.id().clone()),
            },
        )],
    };
    let result = git::push_branches(
        tx.repo_mut(),
        &get_git_repo(&setup.jj_repo),
        "origin",
        &targets,
        git::RemoteCallbacks::default(),
    );
    assert_eq!(result, Ok(()));

    // Check that the ref got updated in the source repo
    let source_repo = git2::Repository::open(&setup.source_repo_dir).unwrap();
    let new_target = source_repo
        .find_reference("refs/heads/main")
        .unwrap()
        .target();
    assert_eq!(new_target, Some(git_id(&setup.sideways_commit)));
}

// TODO(ilyagr): More tests for push safety checks were originally planned. We
// may want to add tests for when a bookmark unexpectedly moved backwards or
// unexpectedly does not exist for bookmark deletion.

#[test]
fn test_push_updates_unexpectedly_moved_sideways_on_remote() {
    let settings = testutils::user_settings();
    let temp_dir = testutils::new_temp_dir();
    let setup = set_up_push_repos(&settings, &temp_dir);

    // The main bookmark is actually at `main_commit` on the remote. If we expect
    // it to be at `sideways_commit`, it unexpectedly moved sideways from our
    // perspective.
    //
    // We cannot delete it or move it anywhere else. However, "moving" it to the
    // same place it already is is OK, following the behavior in
    // `test_merge_ref_targets`.
    //
    // For each test, we check that the push succeeds if and only if the bookmark
    // conflict `jj git fetch` would generate resolves to the push destination.

    let attempt_push_expecting_sideways = |target: Option<CommitId>| {
        let targets = [GitRefUpdate {
            qualified_name: "refs/heads/main".to_string(),
            expected_current_target: Some(setup.sideways_commit.id().clone()),
            new_target: target,
        }];
        git::push_updates(
            setup.jj_repo.as_ref(),
            &get_git_repo(&setup.jj_repo),
            "origin",
            &targets,
            git::RemoteCallbacks::default(),
        )
    };

    assert_matches!(
        attempt_push_expecting_sideways(None),
        Err(GitPushError::RefInUnexpectedLocation(_))
    );

    assert_matches!(
        attempt_push_expecting_sideways(Some(setup.child_of_main_commit.id().clone())),
        Err(GitPushError::RefInUnexpectedLocation(_))
    );

    // Here, the local bookmark hasn't moved from `sideways_commit` from our
    // perspective, but it moved to `main` on the remote. So, the conflict
    // resolves to `main`.
    //
    // `jj` should not actually attempt a push in this case, but if it did, the
    // push should fail.
    assert_matches!(
        attempt_push_expecting_sideways(Some(setup.sideways_commit.id().clone())),
        Err(GitPushError::RefInUnexpectedLocation(_))
    );

    assert_matches!(
        attempt_push_expecting_sideways(Some(setup.parent_of_main_commit.id().clone())),
        Err(GitPushError::RefInUnexpectedLocation(_))
    );

    // Moving the bookmark to the same place it already is is OK.
    assert_eq!(
        attempt_push_expecting_sideways(Some(setup.main_commit.id().clone())),
        Ok(())
    );
}

#[test]
fn test_push_updates_unexpectedly_moved_forward_on_remote() {
    let settings = testutils::user_settings();
    let temp_dir = testutils::new_temp_dir();
    let setup = set_up_push_repos(&settings, &temp_dir);

    // The main bookmark is actually at `main_commit` on the remote. If we
    // expected it to be at `parent_of_commit`, it unexpectedly moved forward
    // from our perspective.
    //
    // We cannot delete it or move it sideways. (TODO: Moving it backwards is
    // also disallowed; there is currently no test for this). However, "moving"
    // it *forwards* is OK. This is allowed *only* in this test, i.e. if the
    // actual location is the descendant of the expected location, and the new
    // location is the descendant of that.
    //
    // For each test, we check that the push succeeds if and only if the bookmark
    // conflict `jj git fetch` would generate resolves to the push destination.

    let attempt_push_expecting_parent = |target: Option<CommitId>| {
        let targets = [GitRefUpdate {
            qualified_name: "refs/heads/main".to_string(),
            expected_current_target: Some(setup.parent_of_main_commit.id().clone()),
            new_target: target,
        }];
        git::push_updates(
            setup.jj_repo.as_ref(),
            &get_git_repo(&setup.jj_repo),
            "origin",
            &targets,
            git::RemoteCallbacks::default(),
        )
    };

    assert_matches!(
        attempt_push_expecting_parent(None),
        Err(GitPushError::RefInUnexpectedLocation(_))
    );

    assert_matches!(
        attempt_push_expecting_parent(Some(setup.sideways_commit.id().clone())),
        Err(GitPushError::RefInUnexpectedLocation(_))
    );

    // Here, the local bookmark hasn't moved from `parent_of_main_commit`, but it
    // moved to `main` on the remote. So, the conflict resolves to `main`.
    //
    // `jj` should not actually attempt a push in this case, but if it did, the push
    // should fail.
    assert_matches!(
        attempt_push_expecting_parent(Some(setup.parent_of_main_commit.id().clone())),
        Err(GitPushError::RefInUnexpectedLocation(_))
    );

    // Moving the bookmark *forwards* is OK, as an exception matching our bookmark
    // conflict resolution rules
    assert_eq!(
        attempt_push_expecting_parent(Some(setup.child_of_main_commit.id().clone())),
        Ok(())
    );
}

#[test]
fn test_push_updates_unexpectedly_exists_on_remote() {
    let settings = testutils::user_settings();
    let temp_dir = testutils::new_temp_dir();
    let setup = set_up_push_repos(&settings, &temp_dir);

    // The main bookmark is actually at `main_commit` on the remote. In this test,
    // we expect it to not exist on the remote at all.
    //
    // We cannot move the bookmark backwards or sideways, but we *can* move it
    // forward (as a special case).
    //
    // For each test, we check that the push succeeds if and only if the bookmark
    // conflict `jj git fetch` would generate resolves to the push destination.

    let attempt_push_expecting_absence = |target: Option<CommitId>| {
        let targets = [GitRefUpdate {
            qualified_name: "refs/heads/main".to_string(),
            expected_current_target: None,
            new_target: target,
        }];
        git::push_updates(
            setup.jj_repo.as_ref(),
            &get_git_repo(&setup.jj_repo),
            "origin",
            &targets,
            git::RemoteCallbacks::default(),
        )
    };

    assert_matches!(
        attempt_push_expecting_absence(Some(setup.parent_of_main_commit.id().clone())),
        Err(GitPushError::RefInUnexpectedLocation(_))
    );

    // We *can* move the bookmark forward even if we didn't expect it to exist
    assert_eq!(
        attempt_push_expecting_absence(Some(setup.child_of_main_commit.id().clone())),
        Ok(())
    );
}

#[test]
fn test_push_updates_success() {
    let settings = testutils::user_settings();
    let temp_dir = testutils::new_temp_dir();
    let setup = set_up_push_repos(&settings, &temp_dir);
    let clone_repo = get_git_repo(&setup.jj_repo);
    let result = git::push_updates(
        setup.jj_repo.as_ref(),
        &clone_repo,
        "origin",
        &[GitRefUpdate {
            qualified_name: "refs/heads/main".to_string(),
            expected_current_target: Some(setup.main_commit.id().clone()),
            new_target: Some(setup.child_of_main_commit.id().clone()),
        }],
        git::RemoteCallbacks::default(),
    );
    assert_eq!(result, Ok(()));

    // Check that the ref got updated in the source repo
    let source_repo = git2::Repository::open(&setup.source_repo_dir).unwrap();
    let new_target = source_repo
        .find_reference("refs/heads/main")
        .unwrap()
        .target();
    let new_oid = git_id(&setup.child_of_main_commit);
    assert_eq!(new_target, Some(new_oid));

    // Check that the ref got updated in the cloned repo. This just tests our
    // assumptions about libgit2 because we want the refs/remotes/origin/main
    // bookmark to be updated.
    let new_target = clone_repo
        .find_reference("refs/remotes/origin/main")
        .unwrap()
        .target();
    assert_eq!(new_target, Some(new_oid));
}

#[test]
fn test_push_updates_no_such_remote() {
    let settings = testutils::user_settings();
    let temp_dir = testutils::new_temp_dir();
    let setup = set_up_push_repos(&settings, &temp_dir);
    let result = git::push_updates(
        setup.jj_repo.as_ref(),
        &get_git_repo(&setup.jj_repo),
        "invalid-remote",
        &[GitRefUpdate {
            qualified_name: "refs/heads/main".to_string(),
            expected_current_target: Some(setup.main_commit.id().clone()),
            new_target: Some(setup.child_of_main_commit.id().clone()),
        }],
        git::RemoteCallbacks::default(),
    );
    assert!(matches!(result, Err(GitPushError::NoSuchRemote(_))));
}

#[test]
fn test_push_updates_invalid_remote() {
    let settings = testutils::user_settings();
    let temp_dir = testutils::new_temp_dir();
    let setup = set_up_push_repos(&settings, &temp_dir);
    let result = git::push_updates(
        setup.jj_repo.as_ref(),
        &get_git_repo(&setup.jj_repo),
        "http://invalid-remote",
        &[GitRefUpdate {
            qualified_name: "refs/heads/main".to_string(),
            expected_current_target: Some(setup.main_commit.id().clone()),
            new_target: Some(setup.child_of_main_commit.id().clone()),
        }],
        git::RemoteCallbacks::default(),
    );
    assert!(matches!(result, Err(GitPushError::NoSuchRemote(_))));
}

#[test]
fn test_bulk_update_extra_on_import_refs() {
    let settings = testutils::user_settings();
    let git_settings = GitSettings::default();
    let test_repo = TestRepo::init_with_backend(TestRepoBackend::Git);
    let repo = &test_repo.repo;
    let git_repo = get_git_repo(repo);

    let count_extra_tables = || {
        let extra_dir = test_repo.repo_path().join("store").join("extra");
        extra_dir
            .read_dir()
            .unwrap()
            .filter(|entry| entry.as_ref().unwrap().metadata().unwrap().is_file())
            .count()
    };
    let import_refs = |repo: &Arc<ReadonlyRepo>| {
        let mut tx = repo.start_transaction(&settings);
        git::import_refs(tx.repo_mut(), &git_settings).unwrap();
        tx.repo_mut().rebase_descendants(&settings).unwrap();
        tx.commit("test").unwrap()
    };

    // Extra metadata table shouldn't be created per read_commit() call. The number
    // of the table files should be way smaller than the number of the heads.
    let mut commit = empty_git_commit(&git_repo, "refs/heads/main", &[]);
    for _ in 1..10 {
        commit = empty_git_commit(&git_repo, "refs/heads/main", &[&commit]);
    }
    let repo = import_refs(repo);
    assert_eq!(count_extra_tables(), 2); // empty + imported_heads == 2

    // Noop import shouldn't create a table file.
    let repo = import_refs(&repo);
    assert_eq!(count_extra_tables(), 2);

    // Importing new head should add exactly one table file.
    for _ in 0..10 {
        commit = empty_git_commit(&git_repo, "refs/heads/main", &[&commit]);
    }
    let repo = import_refs(&repo);
    assert_eq!(count_extra_tables(), 3);

    drop(repo); // silence clippy
}

#[test]
fn test_rewrite_imported_commit() {
    let settings = testutils::user_settings();
    let git_settings = GitSettings::default();
    let test_repo = TestRepo::init_with_backend(TestRepoBackend::Git);
    let repo = &test_repo.repo;
    let git_repo = get_git_repo(repo);

    // Import git commit, which generates change id from the commit id.
    let git_commit = empty_git_commit(&git_repo, "refs/heads/main", &[]);
    let mut tx = repo.start_transaction(&settings);
    git::import_refs(tx.repo_mut(), &git_settings).unwrap();
    tx.repo_mut().rebase_descendants(&settings).unwrap();
    let repo = tx.commit("test").unwrap();
    let imported_commit = repo.store().get_commit(&jj_id(&git_commit)).unwrap();

    // Try to create identical commit with different change id.
    let mut tx = repo.start_transaction(&settings);
    let authored_commit = tx
        .repo_mut()
        .new_commit(
            &settings,
            imported_commit.parent_ids().to_vec(),
            imported_commit.tree_id().clone(),
        )
        .set_author(imported_commit.author().clone())
        .set_committer(imported_commit.committer().clone())
        .set_description(imported_commit.description())
        .write()
        .unwrap();
    let repo = tx.commit("test").unwrap();

    // Imported commit shouldn't be reused, and the timestamp of the authored
    // commit should be adjusted to create new commit.
    assert_ne!(imported_commit.id(), authored_commit.id());
    assert_ne!(
        imported_commit.committer().timestamp,
        authored_commit.committer().timestamp,
    );

    // The index should be consistent with the store.
    assert_eq!(
        repo.resolve_change_id(imported_commit.change_id()),
        Some(vec![imported_commit.id().clone()]),
    );
    assert_eq!(
        repo.resolve_change_id(authored_commit.change_id()),
        Some(vec![authored_commit.id().clone()]),
    );
}

#[test]
fn test_concurrent_write_commit() {
    let settings = &testutils::user_settings();
    let test_repo = TestRepo::init_with_backend(TestRepoBackend::Git);
    let test_env = &test_repo.env;
    let repo = &test_repo.repo;

    // Try to create identical commits with different change ids. Timestamp of the
    // commits should be adjusted such that each commit has a unique commit id.
    let num_thread = 8;
    let (sender, receiver) = mpsc::channel();
    thread::scope(|s| {
        let barrier = Arc::new(Barrier::new(num_thread));
        for i in 0..num_thread {
            let repo = test_env.load_repo_at_head(settings, test_repo.repo_path()); // unshare loader
            let barrier = barrier.clone();
            let sender = sender.clone();
            s.spawn(move || {
                barrier.wait();
                let mut tx = repo.start_transaction(settings);
                let commit = create_rooted_commit(tx.repo_mut(), settings)
                    .set_description("racy commit")
                    .write()
                    .unwrap();
                tx.commit(format!("writer {i}")).unwrap();
                sender
                    .send((commit.id().clone(), commit.change_id().clone()))
                    .unwrap();
            });
        }
    });

    drop(sender);
    let mut commit_change_ids: BTreeMap<CommitId, HashSet<ChangeId>> = BTreeMap::new();
    for (commit_id, change_id) in receiver {
        commit_change_ids
            .entry(commit_id)
            .or_default()
            .insert(change_id);
    }

    // Ideally, each commit should have unique commit/change ids.
    assert_eq!(commit_change_ids.len(), num_thread);

    // All unique commits should be preserved.
    let repo = repo.reload_at_head(settings).unwrap();
    for (commit_id, change_ids) in &commit_change_ids {
        let commit = repo.store().get_commit(commit_id).unwrap();
        assert_eq!(commit.id(), commit_id);
        assert!(change_ids.contains(commit.change_id()));
    }

    // The index should be consistent with the store.
    for commit_id in commit_change_ids.keys() {
        assert!(repo.index().has_id(commit_id));
        let commit = repo.store().get_commit(commit_id).unwrap();
        assert_eq!(
            repo.resolve_change_id(commit.change_id()),
            Some(vec![commit_id.clone()]),
        );
    }
}

#[test]
fn test_concurrent_read_write_commit() {
    let settings = &testutils::user_settings();
    let test_repo = TestRepo::init_with_backend(TestRepoBackend::Git);
    let test_env = &test_repo.env;
    let repo = &test_repo.repo;

    // Create unique commits and load them concurrently. In this test, we assume
    // that writer doesn't fall back to timestamp adjustment, so the expected
    // commit ids are static. If reader could interrupt in the timestamp
    // adjustment loop, this assumption wouldn't apply.
    let commit_ids = [
        "c5c6efd6ac240102e7f047234c3cade55eedd621",
        "9f7a96a6c9d044b228f3321a365bdd3514e6033a",
        "aa7867ad0c566df5bbb708d8d6ddc88eefeea0ff",
        "930a76e333d5cc17f40a649c3470cb99aae24a0c",
        "88e9a719df4f0cc3daa740b814e271341f6ea9f4",
        "4883bdc57448a53b4eef1af85e34b85b9ee31aee",
        "308345f8d058848e83beed166704faac2ecd4541",
        "9e35ff61ea8d1d4ef7f01edc5fd23873cc301b30",
        "8010ac8c65548dd619e7c83551d983d724dda216",
        "bbe593d556ea31acf778465227f340af7e627b2b",
        "2f6800f4b8e8fc4c42dc0e417896463d13673654",
        "a3a7e4fcddeaa11bb84f66f3428f107f65eb3268",
        "96e17ff3a7ee1b67ddfa5619b2bf5380b80f619a",
        "34613f7609524c54cc990ada1bdef3dcad0fd29f",
        "95867e5aed6b62abc2cd6258da9fee8873accfd3",
        "7635ce107ae7ba71821b8cd74a1405ca6d9e49ac",
    ]
    .into_iter()
    .map(CommitId::from_hex)
    .collect_vec();
    let num_reader_thread = 8;
    thread::scope(|s| {
        let barrier = Arc::new(Barrier::new(commit_ids.len() + num_reader_thread));

        // Writer assigns random change id
        for (i, commit_id) in commit_ids.iter().enumerate() {
            let repo = test_env.load_repo_at_head(settings, test_repo.repo_path()); // unshare loader
            let barrier = barrier.clone();
            s.spawn(move || {
                barrier.wait();
                let mut tx = repo.start_transaction(settings);
                let commit = create_rooted_commit(tx.repo_mut(), settings)
                    .set_description(format!("commit {i}"))
                    .write()
                    .unwrap();
                tx.commit(format!("writer {i}")).unwrap();
                assert_eq!(commit.id(), commit_id);
            });
        }

        // Reader may generate change id (if not yet assigned by the writer)
        for i in 0..num_reader_thread {
            let mut repo = test_env.load_repo_at_head(settings, test_repo.repo_path()); // unshare loader
            let barrier = barrier.clone();
            let mut pending_commit_ids = commit_ids.clone();
            pending_commit_ids.rotate_left(i); // start lookup from different place
            s.spawn(move || {
                barrier.wait();
                // This loop should finish within a couple of retries, but terminate in case
                // it doesn't.
                for _ in 0..100 {
                    if pending_commit_ids.is_empty() {
                        break;
                    }
                    repo = repo.reload_at_head(settings).unwrap();
                    let git_backend = get_git_backend(&repo);
                    let mut tx = repo.start_transaction(settings);
                    pending_commit_ids = pending_commit_ids
                        .into_iter()
                        .filter_map(|commit_id| {
                            match git_backend.import_head_commits([&commit_id]) {
                                Ok(()) => {
                                    // update index as git::import_refs() would do
                                    let commit = repo.store().get_commit(&commit_id).unwrap();
                                    tx.repo_mut().add_head(&commit).unwrap();
                                    None
                                }
                                Err(BackendError::ObjectNotFound { .. }) => Some(commit_id),
                                Err(err) => {
                                    eprintln!(
                                        "import error in reader {i} (maybe lock contention?): {}",
                                        iter::successors(
                                            Some(&err as &dyn std::error::Error),
                                            |e| e.source(),
                                        )
                                        .join(": ")
                                    );
                                    Some(commit_id)
                                }
                            }
                        })
                        .collect_vec();
                    if tx.repo_mut().has_changes() {
                        tx.commit(format!("reader {i}")).unwrap();
                    }
                    thread::yield_now();
                }
                if !pending_commit_ids.is_empty() {
                    // It's not an error if some of the readers couldn't observe the commits. It's
                    // unlikely, but possible if the git backend had strong negative object cache
                    // for example.
                    eprintln!(
                        "reader {i} couldn't observe the following commits: \
                         {pending_commit_ids:#?}"
                    );
                }
            });
        }
    });

    // The index should be consistent with the store.
    let repo = repo.reload_at_head(settings).unwrap();
    for commit_id in &commit_ids {
        assert!(repo.index().has_id(commit_id));
        let commit = repo.store().get_commit(commit_id).unwrap();
        assert_eq!(
            repo.resolve_change_id(commit.change_id()),
            Some(vec![commit_id.clone()]),
        );
    }
}

fn create_rooted_commit<'repo>(
    mut_repo: &'repo mut MutableRepo,
    settings: &UserSettings,
) -> CommitBuilder<'repo> {
    let signature = Signature {
        name: "Test User".to_owned(),
        email: "test.user@example.com".to_owned(),
        timestamp: Timestamp {
            // avoid underflow during timestamp adjustment
            timestamp: MillisSinceEpoch(1_000_000),
            tz_offset: 0,
        },
    };
    mut_repo
        .new_commit(
            settings,
            vec![mut_repo.store().root_commit_id().clone()],
            mut_repo.store().empty_merged_tree_id(),
        )
        .set_author(signature.clone())
        .set_committer(signature)
}

#[test]
fn test_parse_gitmodules() {
    let result = git::parse_gitmodules(
        &mut r#"
[submodule "wellformed"]
url = https://github.com/martinvonz/jj
path = mod
update = checkout # Extraneous config

[submodule "uppercase"]
URL = https://github.com/martinvonz/jj
PATH = mod2

[submodule "repeated_keys"]
url = https://github.com/martinvonz/jj
path = mod3
url = https://github.com/chooglen/jj
path = mod4

# The following entries aren't expected in a well-formed .gitmodules
[submodule "missing_url"]
path = mod

[submodule]
ignoreThisSection = foo

[randomConfig]
ignoreThisSection = foo
"#
        .as_bytes(),
    )
    .unwrap();
    let expected = btreemap! {
        "wellformed".to_string() => SubmoduleConfig {
            name: "wellformed".to_string(),
            url: "https://github.com/martinvonz/jj".to_string(),
            path: "mod".to_string(),
        },
        "uppercase".to_string() => SubmoduleConfig {
            name: "uppercase".to_string(),
            url: "https://github.com/martinvonz/jj".to_string(),
            path: "mod2".to_string(),
        },
        "repeated_keys".to_string() => SubmoduleConfig {
            name: "repeated_keys".to_string(),
            url: "https://github.com/martinvonz/jj".to_string(),
            path: "mod3".to_string(),
        },
    };

    assert_eq!(result, expected);
}

#[test]
fn test_shallow_commits_lack_parents() {
    let settings = testutils::user_settings();
    let test_repo = TestRepo::init_with_backend(TestRepoBackend::Git);
    let test_env = &test_repo.env;
    let repo = &test_repo.repo;
    let git_repo = get_git_repo(repo);

    // D   E (`main`)
    // |   |
    // B   C // shallow boundary
    // | /
    // A
    // |
    // git_root
    let git_root = empty_git_commit(&git_repo, "refs/heads/main", &[]);

    let a = empty_git_commit(&git_repo, "refs/heads/main", &[&git_root]);

    let b = empty_git_commit(&git_repo, "refs/heads/feature", &[&a]);
    let c = empty_git_commit(&git_repo, "refs/heads/main", &[&a]);

    let d = empty_git_commit(&git_repo, "refs/heads/feature", &[&b]);
    let e = empty_git_commit(&git_repo, "refs/heads/main", &[&c]);

    git_repo.set_head("refs/heads/main").unwrap();

    let make_shallow = |repo, mut shallow_commits: Vec<_>| {
        let shallow_file = get_git_backend(repo).git_repo().shallow_file();
        shallow_commits.sort();
        let mut buf = Vec::<u8>::new();
        for commit in shallow_commits {
            writeln!(buf, "{commit}").unwrap();
        }
        fs::write(shallow_file, buf).unwrap();
        // Reload the repo to invalidate mtime-based in-memory cache
        test_env.load_repo_at_head(&settings, test_repo.repo_path())
    };
    let repo = make_shallow(repo, vec![b.id(), c.id()]);

    let mut tx = repo.start_transaction(&settings);
    git::import_refs(tx.repo_mut(), &GitSettings::default()).unwrap();
    let repo = tx.commit("import").unwrap();
    let store = repo.store();
    let root = store.root_commit_id();

    let expected_heads = hashset! {
        jj_id(&d),
        jj_id(&e),
    };
    assert_eq!(*repo.view().heads(), expected_heads);

    let parents = |store: &Arc<jj_lib::store::Store>, commit| {
        let commit = store.get_commit(&jj_id(commit)).unwrap();
        commit.parent_ids().to_vec()
    };

    assert_eq!(
        parents(store, &b),
        vec![root.clone()],
        "shallow commits have the root commit as a parent"
    );
    assert_eq!(
        parents(store, &c),
        vec![root.clone()],
        "shallow commits have the root commit as a parent"
    );

    // deepen the shallow clone
    let repo = make_shallow(&repo, vec![a.id()]);

    let mut tx = repo.start_transaction(&settings);
    git::import_refs(tx.repo_mut(), &GitSettings::default()).unwrap();
    let repo = tx.commit("import").unwrap();
    let store = repo.store();
    let root = store.root_commit_id();

    assert_eq!(
        parents(store, &a),
        vec![root.clone()],
        "shallow commits have the root commit as a parent"
    );
    assert_eq!(
        parents(store, &b),
        vec![jj_id(&a)],
        "unshallowed commits have parents"
    );
    assert_eq!(
        parents(store, &c),
        vec![jj_id(&a)],
        "unshallowed commits have correct parents"
    );
    // FIXME: new ancestors should be indexed
    assert!(!repo.index().has_id(&jj_id(&a)));
}
