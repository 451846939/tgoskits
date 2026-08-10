#!/usr/bin/env bash
# Copyright 2026 The TGOSKits Authors
#
# SPDX-License-Identifier: Apache-2.0

third_party_source_status() {
  local source_dir="$1"
  local line

  while IFS= read -r line; do
    # Finder metadata is not a source change and may be recreated outside Git.
    if [[ "${line}" == "?? .DS_Store" || "${line}" == "?? "*"/.DS_Store" ]]; then
      continue
    fi
    printf '%s\n' "${line}"
  done < <(git -C "${source_dir}" status --porcelain --untracked-files=all)
}

third_party_assert_git_source() {
  local name="$1"
  local source_dir="$2"
  local required_ref="${3:-}"
  local actual_commit required_commit dirty

  if [[ ! -d "${source_dir}" ]] || \
     ! git -C "${source_dir}" rev-parse --is-inside-work-tree >/dev/null 2>&1; then
    echo "ERROR: ${name} source is not a Git worktree: ${source_dir}" >&2
    return 1
  fi

  actual_commit="$(git -C "${source_dir}" rev-parse HEAD)"
  if [[ -n "${required_ref}" ]]; then
    if ! required_commit="$(git -C "${source_dir}" rev-parse "${required_ref}^{commit}" 2>/dev/null)"; then
      echo "ERROR: ${name} required ref does not exist: ${required_ref}" >&2
      return 1
    fi
    if [[ "${actual_commit}" != "${required_commit}" ]]; then
      echo "ERROR: ${name} source is not at ${required_ref}." >&2
      echo "  expected: ${required_commit}" >&2
      echo "  actual:   ${actual_commit}" >&2
      return 1
    fi
  fi

  dirty="$(third_party_source_status "${source_dir}")"
  if [[ -n "${dirty}" ]]; then
    echo "ERROR: ${name} third-party source tree has local changes:" >&2
    printf '%s\n' "${dirty}" >&2
    echo "Move the adaptation into TGOSKits-owned app, overlay, config, or build files." >&2
    return 1
  fi

  echo "[ai-rtos] third-party source clean: ${name} commit=${actual_commit}"
}

third_party_assert_nested_git_clean() {
  local name="$1"
  local workspace_root="$2"
  local marker repo dirty
  local failed=0

  if [[ ! -d "${workspace_root}" ]]; then
    echo "ERROR: ${name} workspace does not exist: ${workspace_root}" >&2
    return 1
  fi

  while IFS= read -r -d '' marker; do
    repo="${marker%/.git}"
    dirty="$(third_party_source_status "${repo}")"
    if [[ -n "${dirty}" ]]; then
      echo "ERROR: dirty third-party repository: ${repo}" >&2
      printf '%s\n' "${dirty}" >&2
      failed=1
    fi
  done < <(find "${workspace_root}" -name .git -prune -print0)

  if (( failed != 0 )); then
    return 1
  fi
  echo "[ai-rtos] third-party workspace clean: ${name} root=${workspace_root}"
}
