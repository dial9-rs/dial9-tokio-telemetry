# Contributing Guidelines

Thank you for your interest in contributing to our project. Whether it's a bug report, new feature, correction, or additional documentation, we greatly value feedback and contributions from our community.

Please read through this document before submitting any issues or pull requests to ensure we have all the necessary information to effectively respond to your bug report or contribution.

## Reporting Bugs/Feature Requests

We welcome you to use the GitHub issue tracker to report bugs or suggest features.

When filing an issue, please check [existing open](https://github.com/dial9-rs/dial9-tokio-telemetry/issues), or [recently closed](https://github.com/dial9-rs/dial9-tokio-telemetry/issues?utf8=%E2%9C%93&q=is%3Aissue%20is%3Aclosed%20), issues to make sure somebody else hasn't already reported the issue. Please try to include as much information as you can. Details like these are incredibly useful:

* A reproducible test case or series of steps
* The version of our code being used
* Any modifications you've made relevant to the bug
* Anything unusual about your environment or deployment

## Contributing via Pull Requests

Contributions via pull requests are much appreciated. Before sending us a pull request, please ensure that:

1. You are working against the latest source on the *main* branch.
2. You check existing open, and recently merged, pull requests to make sure someone else hasn't addressed the problem already.
3. You open an issue to discuss any significant work, we would hate for your time to be wasted.

To send us a pull request, please:

1. Fork the repository.
2. Modify the source; please focus on the specific change you are contributing. If you also reformat all the code, it will be hard for us to focus on your change.
3. Ensure local tests pass.
4. Commit to your fork using clear commit messages and ensure any Rust source files have been formatted with the [rustfmt tool](https://github.com/rust-lang/rustfmt#quick-start)
5. Send us a pull request, answering any default questions in the pull request interface.
6. Pay attention to any automated CI failures reported in the pull request, and stay involved in the conversation.

GitHub provides additional document on [forking a repository](https://help.github.com/articles/fork-a-repo/) and [creating a pull request](https://help.github.com/articles/creating-a-pull-request/).

## Finding contributions to work on

Looking at the existing issues is a great way to find something to contribute on. As our projects, by default, use the default GitHub issue labels (enhancement/bug/duplicate/help wanted/invalid/question/wontfix), looking at any ['help wanted'](https://github.com/dial9-rs/dial9-tokio-telemetry/labels/help%20wanted) issues is a great place to start.

## Dependencies on crates within the workspace

Within-workspace crate dependencies are managed centrally in the root `Cargo.toml` under `[workspace.dependencies]`, with both a `path` and a `version`:

```toml
# root Cargo.toml
[workspace.dependencies]
dial9-trace-format = { version = "0.3.2", path = "dial9-trace-format" }
```

Crates then reference these with `workspace = true`:

```toml
# dial9-tokio-telemetry/Cargo.toml
[dependencies]
dial9-trace-format = { workspace = true, features = ["serde"] }
```

The `version` in the workspace dependency is required for publishing. `release-plz` updates these versions automatically during releases.

Dev-dependencies on workspace crates should *not* include a `version` to avoid chicken-and-egg problems when publishing (since `release-plz` might update the version to the one you are currently publishing):

```toml
[dev-dependencies]
dial9-tokio-telemetry = { path = ".", features = ["analysis", "tracing-layer"] }
```

## Running tests
Some tests will only run with the `shuttle` cfg enabled. There is a script to run these: `scripts/test-shuttle.sh`.

For other tests, `cargo nextest run` will run all of the normal tests.

## Doing releases

Releases are human-initiated, not automatic. The process has two parts:

### How it works

1. **Release PR (automatic):** On every push to `main`, the `release-pr.yml` workflow runs `release-plz release-pr`, which creates/updates a PR with version bumps and changelog entries based on [conventional commits]. This PR accumulates over time — you can merge many feature PRs before releasing.

2. **Publishing (manual):** When you're ready to release, merge the release PR, then go to **Actions → "Publish release" → Run workflow** and click "Run". The `environment: release` gate requires approval before publishing proceeds.

The `release.yml` workflow is authorized to publish releases to the dial9 crates via [trusted publishing]. No tokens need to be managed.

[trusted publishing]: https://rust-lang.github.io/rfcs/3691-trusted-publishing-cratesio.html
[conventional commits]: https://www.conventionalcommits.org/en/v1.0.0/

### Step by step

1. Merge your PRs to `main` using conventional commit messages (e.g. `feat:`, `fix:`, `feat!:` for breaking changes).
2. The release PR will update automatically. Review the changelog and version bumps.
3. If you need to adjust versions (e.g. force a major bump), edit `Cargo.toml` versions in the release PR before merging.
4. Merge the release PR.
5. Go to Actions → "Publish release" → Run workflow → confirm.
6. A team member approves the deployment in the `release` environment.

### Semver checks

`cargo-semver-checks` runs on every PR as an advisory check. It won't block merge, but if it reports breaking changes, ensure the release PR reflects a major version bump before publishing.

### Breaking changes

You can freely merge breaking changes to `main`. The release PR will accumulate them. Before publishing, verify that `release-plz` has bumped the major version (it runs `semver_check = true` and should do this automatically). If it hasn't, manually adjust the version in the release PR.

### Publishing a new crate

trusted publishing is unable to publish new crates. If you want to add a new crate to the dial9 family, you should:

1. create a branch that contains the crate you are publishing (it should be in the root `Cargo.toml`'s `workspace.members`, and in a publishable state).
2. add the package name to the `changelog_include` list in the `[[package]] name = "dial9-tokio-telemetry"` entry in `release-plz.toml`.
3. run `cargo publish -p <package> --dry-run`
4. get a temporary crates.io token just for the publishing
5. run `cargo login` with that token
6. run `cargo publish -p <package>`
7. set up trusted publishing via the crates.io WebUI to the following state:

   ```
   Publisher: Github
   Repository: dial9-rs/dial9-tokio-telemetry
   Workflow: release.yml
   Environment: release
   ```

8. revoke the temporary crates.io token

Further publishing should happen via release-plz, without needing to manually work with tokens.

## Licensing

See the [LICENSE](https://github.com/dial9-rs/dial9-tokio-telemetry/blob/main/LICENSE) file for our project's licensing.
