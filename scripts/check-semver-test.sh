#!/bin/bash

PR="${1:-42}"
BASE_BRANCH="${2:-stable2503}"

echo "PR number: $PR"
echo "Base branch: $BASE_BRANCH"
grep description "prdoc/pr_$PR.prdoc"

          # Only enforce SemVer restrictions for backports targeting stable branches
          if [[ "$BASE_BRANCH" != stable* ]]; then
              echo "ℹ️ Branch '$BASE_BRANCH' is not a stable branch. Skipping SemVer check."
              exit 0
          fi

          echo "🔍 Backport branch detected, checking for disallowed semver changes..."

          prdoc_file="prdoc/pr_$PR.prdoc"

          # Check if there are any major bumps
          if ! grep -q "bump:[[:space:]]*major" "$prdoc_file"; then
              echo "✅ All semver changes in backport are valid (minor, patch, or none)."
              exit 0
          fi

          # Process each major bump and check the next line
          temp_file=$(mktemp)
          grep -A1 "bump:[[:space:]]*major" "$prdoc_file" > "$temp_file"

          while read -r line; do
              if [[ "$line" =~ bump:[[:space:]]*major ]]; then
                  # This is the bump line, read the next line
                  read -r next_line
                  if [[ "$next_line" =~ validate:[[:space:]]*false ]]; then
                      continue  # This major bump is properly validated
                  else
                      echo "❌ Error: Found major bump without 'validate: false'"
                      echo "📘 See: https://github.com/paritytech/polkadot-sdk/blob/master/docs/contributor/prdoc.md#backporting-prs"
                      echo "🔧 Add 'validate: false' after the major bump in $prdoc_file with justification."
                      rm -f "$temp_file"
                      exit 1
                  fi
              fi
          done < "$temp_file"

          rm -f "$temp_file"

          # If we reach here, all major bumps have validate: false
          echo "⚠️ Backport contains major bumps, but they are all marked with validate: false."
          echo "✅ Semver override accepted. Please ensure justification is documented in the PR description."