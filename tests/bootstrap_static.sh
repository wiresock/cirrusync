#!/usr/bin/env bash

set -Eeuo pipefail
IFS=$'\n\t'

REPOSITORY_ROOT="$(
    cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." >/dev/null
    pwd -P
)"
readonly REPOSITORY_ROOT
readonly BOOTSTRAP="${REPOSITORY_ROOT}/bootstrap.sh"
readonly UNIT="${REPOSITORY_ROOT}/systemd/cirrusync.service"

fail() {
    printf 'bootstrap static test failed: %s\n' "$*" >&2
    exit 1
}

assert_contains() {
    local file="$1"
    local expected="$2"

    grep -Fq -- "${expected}" "${file}" ||
        fail "${file} does not contain expected text: ${expected}"
}

assert_not_contains() {
    local file="$1"
    local unexpected="$2"

    if grep -Fq -- "${unexpected}" "${file}"; then
        fail "${file} contains forbidden text: ${unexpected}"
    fi
}

bash -n "${BOOTSTRAP}"

assert_contains "${BOOTSTRAP}" 'readonly BUILD_USER="cirrusync-build"'
assert_contains "${BOOTSTRAP}" 'readonly TRUSTED_PATH='
assert_not_contains "${BOOTSTRAP}" 'local build_user="nobody"'
assert_contains "${BOOTSTRAP}" 'read_from_tty'
assert_contains "${BOOTSTRAP}" 'IFS= read -r "$@" </dev/tty'
assert_contains "${BOOTSTRAP}" 'CIRRUSYNC_ACCOUNT_ID'
assert_contains "${BOOTSTRAP}" 'Cloudflare account ID (required for account-owned tokens; blank for a user token)'
assert_contains "${BOOTSTRAP}" 'account_id = "%s"'
assert_contains "${BOOTSTRAP}" 'acquire_installer_lock'
assert_contains "${BOOTSTRAP}" 'begin_transaction'
assert_contains "${BOOTSTRAP}" 'rollback_transaction'
assert_contains "${BOOTSTRAP}" 'promote_source'
assert_contains "${BOOTSTRAP}" 'validate_local_system_user'
assert_contains "${BOOTSTRAP}" 'validate_local_system_group'
assert_contains "${BOOTSTRAP}" 'parent_mode="$(stat -Lc'
assert_contains "${BOOTSTRAP}" 'primary group of an unexpected local user'
assert_contains "${BOOTSTRAP}" 'must have a locked password and no administrators or members'
# shellcheck disable=SC2016
assert_contains "${BOOTSTRAP}" 'CONFIG_DIR}" == "${CONFIG_ROOT}'
# shellcheck disable=SC2016
assert_contains "${BOOTSTRAP}" '/proc/${BASHPID}/fd/${token_fd}'
assert_contains "${BOOTSTRAP}" 'validate_trusted_directory_chain "${token_source_parent}"'
assert_contains "${BOOTSTRAP}" 'validate_repository_url "${REPO_URL}"'
assert_contains "${BOOTSTRAP}" 'credential-free HTTPS repository URL'
assert_contains "${BOOTSTRAP}" 'run_without_privilege_gain "${BUILD_USER}" env -i "${git_environment[@]}"'
assert_contains "${BOOTSTRAP}" 'setpriv --no-new-privs'
assert_contains "${BOOTSTRAP}" 'read_cirrusync_binary_version'
assert_contains "${BOOTSTRAP}" 'compare_release_versions'
assert_contains "${BOOTSTRAP}" 'prlimit --fsize=128:128'
assert_contains "${BOOTSTRAP}" 'refusing to downgrade Cirrusync'
assert_contains "${BOOTSTRAP}" 'candidate source changed without a version bump'
assert_contains "${BOOTSTRAP}" 'the release binary version ${built_version} does not match'
assert_contains "${BOOTSTRAP}" '"RUSTC=${RUSTC_BIN}"'
assert_contains "${BOOTSTRAP}" 'validate_existing_token_before_build'
assert_contains "${BOOTSTRAP}" 'validate_configuration_directory_acl'
assert_contains "${BOOTSTRAP}" 'getfacl --absolute-names --numeric --omit-header'
assert_contains "${BOOTSTRAP}" '"GIT_CONFIG_GLOBAL=${GIT_SAFE_CONFIG}"'
assert_contains "${BOOTSTRAP}" 'chown "root:${BUILD_GROUP}" "${GIT_SAFE_CONFIG}"'
assert_not_contains "${BOOTSTRAP}" 'git -c "safe.directory='
assert_contains "${BOOTSTRAP}" '-c protocol.ext.allow=never'
assert_contains "${BOOTSTRAP}" '-c http.followRedirects=false'
assert_contains "${BOOTSTRAP}" 'chown --recursive --no-dereference root:root "${STAGED_SOURCE}"'
assert_contains "${BOOTSTRAP}" 'chmod --recursive u+rwX,go+rX,go-w "${STAGED_SOURCE}"'
assert_contains "${BOOTSTRAP}" 'the staged source contains a hard-linked file'
assert_contains "${BOOTSTRAP}" '--allow-edit-probe --allow-create'
assert_contains "${BOOTSTRAP}" '"https://api6.ipify.org"'
assert_not_contains "${BOOTSTRAP}" '"https://api64.ipify.org"'
assert_not_contains "${BOOTSTRAP}" 'update_on_start = true'
assert_contains "${BOOTSTRAP}" 'record_ttl="1"'
assert_contains "${BOOTSTRAP}" '"${interval}" -ge 60'
assert_contains "${BOOTSTRAP}" 'systemd-analyze verify'
expected_unit_sha256="$(
    sed -n 's/^readonly SYSTEMD_UNIT_SHA256="\([0-9a-f]\{64\}\)"$/\1/p' \
        "${BOOTSTRAP}"
)"
[[ -n "${expected_unit_sha256}" ]] ||
    fail "bootstrap does not contain a trusted unit checksum"
actual_unit_sha256="$(sha256sum "${UNIT}" | awk '{print $1}')"
[[ "${actual_unit_sha256}" == "${expected_unit_sha256}" ]] ||
    fail "trusted systemd unit checksum is stale"
assert_contains "${BOOTSTRAP}" 'refusing repository-controlled systemd privileges'
assert_contains "${BOOTSTRAP}" 'require_running_systemd'
assert_contains "${BOOTSTRAP}" 'service_remains_healthy 5'
assert_contains "${BOOTSTRAP}" 'property=NRestarts'
assert_contains "${BOOTSTRAP}" 'timeout --signal=TERM --kill-after=15s 330s'
assert_contains "${BOOTSTRAP}" 'findmnt --kernel --raw --noheadings --output TARGET'
assert_contains "${BOOTSTRAP}" 'managed source inspection left processes that could not be terminated'
assert_contains "${BOOTSTRAP}" 'verify_service_stopped'
assert_contains "${BOOTSTRAP}" 'refusing to replace runtime files while a process owned by ${SERVICE_USER} is running'
assert_contains "${BOOTSTRAP}" 'configuration inputs are not applied during upgrade; use --reconfigure'
assert_contains "${BOOTSTRAP}" 'remove_system_identity'
assert_contains "${BOOTSTRAP}" 'token source must be root-owned and have exactly one hard link'
assert_contains "${BOOTSTRAP}" 'BOOTSTRAP_IS_SOURCED=false'
assert_contains "${BOOTSTRAP}" 'if [[ "${BOOTSTRAP_IS_SOURCED}" == false ]]'

assert_contains "${UNIT}" 'TimeoutStopSec=150'
assert_contains "${UNIT}" 'DevicePolicy=closed'
assert_contains "${UNIT}" 'KeyringMode=private'
assert_contains "${UNIT}" 'PrivateMounts=true'
assert_contains "${UNIT}" 'RemoveIPC=true'

printf 'bootstrap static checks passed\n'
