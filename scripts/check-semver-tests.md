# Test Summary: `check-semver` Backport Script

Tests validate the script's behaviour across different PR scenarios and base branches, specifically checking SemVer
compliance backport-specific enforcements.

* [`check-semver-test.sh`](check-semver-test.sh)
* [PR #42 - Test 1](../prdoc/pr_42.prdoc)
* [PR #43 - Test 2](../prdoc/pr_43.prdoc)
* [PR #44 - Test 3](../prdoc/pr_44.prdoc)
* [PR #45 - Test 4](../prdoc/pr_45.prdoc)

## Tests on `stable2503` (Stable Branch - SemVer Check Active)

| PR # | Description                        | Script Output Summary                                                                                    | Result                                                                      |
|:-----|:-----------------------------------|:---------------------------------------------------------------------------------------------------------|:----------------------------------------------------------------------------|
| 42   | Test 1 is allowed for backporting. | `Backport contains major bumps, but they are all marked with validate: false. Semver override accepted.` | **OK (Override Accepted)** - Major bump present but explicitly validated.   |
| 43   | Test 2 fails backporting.          | `Error: Found major bump without 'validate: false'`                                                      | **FAIL** - Major bump detected without the required `validate: false` flag. |
| 44   | Test 3 is allowed for backporting. | `All semver changes in backport are valid (minor, patch, or none).`                                      | **OK** - No major SemVer bumps found, so no validation needed.              |
| 45   | Test 4 is allowed for backporting. | `Backport contains major bumps, but they are all marked with validate: false. Semver override accepted.` | **OK (Override Accepted)** - Major bump present but explicitly validated.   |

## Tests on `dev` (Non-Stable Branch - SemVer Check Skipped)

| PR # | Description                        | Script Output Summary                                                                        | Result                                                                          |
|:-----|:-----------------------------------|:---------------------------------------------------------------------------------------------|:--------------------------------------------------------------------------------|
| 42   | Test 1 is allowed for backporting. | `Branch 'dev' is not a stable branch. Skipping SemVer check backport-specific enforcements.` | **Ignored** - Correctly skipped the SemVer check as it's not a `stable` branch. |
| 43   | Test 2 fails backporting.          | `Branch 'dev' is not a stable branch. Skipping SemVer check backport-specific enforcements.` | **Ignored** - Correctly skipped the SemVer check as it's not a `stable` branch. |
| 44   | Test 3 is allowed for backporting. | `Branch 'dev' is not a stable branch. Skipping SemVer check backport-specific enforcements.` | **Ignored** - Correctly skipped the SemVer check as it's not a `stable` branch. |
| 45   | Test 4 is allowed for backporting. | `Branch 'dev' is not a stable branch. Skipping SemVer check backport-specific enforcements.` | **Ignored** - Correctly skipped the SemVer check as it's not a `stable` branch. |

## Key Takeaways

* The script correctly **enforces SemVer checks for branches prefixed with `stable`** (e.g., `stable2503`).
* It successfully **identifies and flags major bumps** that are not explicitly marked with `validate: false` (PR #43).
* It correctly **accepts major bumps** when `validate: false` is present (PR #42, PR #45).
* It **properly skips the SemVer validation** and exits successfully for non-stable branches (e.g., `dev`), as intended.

The test results indicate your simplified `grep`-based logic is working as expected for these scenarios.
