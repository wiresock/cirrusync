#!/usr/bin/env bash

set -Eeuo pipefail
IFS=$'\n\t'
umask 0022

REPOSITORY_ROOT="$(
    cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." >/dev/null
    pwd -P
)"
readonly REPOSITORY_ROOT
readonly FIXTURE_HOST="cirrusync-ci.invalid"
readonly REPOSITORY_URL="https://${FIXTURE_HOST}/repo.git"
readonly INTEGRATION_BRANCH="bootstrap-integration"
readonly TOKEN_VALUE="cirrusync-integration-token"
readonly CONFIG_DIR="/etc/cirrusync"
readonly CONFIG_PATH="/etc/cirrusync/config.toml"
readonly TOKEN_PATH="/etc/cirrusync/token"
readonly SOURCE_DIR="/usr/local/src/cirrusync"
readonly BINARY_PATH="/usr/local/bin/cirrusync"
readonly UNIT_PATH="/etc/systemd/system/cirrusync.service"
readonly FSMONITOR_MARKER="/var/tmp/cirrusync-fsmonitor-probe.uid"
readonly FSMONITOR_PID_FILE="/var/tmp/cirrusync-fsmonitor-probe.pid"
readonly FSMONITOR_CONTAMINATION="/var/tmp/cirrusync-fsmonitor-contaminated"
readonly STATE_DIR="/var/lib/cirrusync"
readonly BUILD_STATE_DIR="/var/lib/cirrusync-build"
readonly TOOLCHAIN_CARGO_LINK="/usr/sbin/cargo"
readonly TOOLCHAIN_RUSTC_LINK="/usr/sbin/rustc"

if ((EUID == 0)); then
    SUDO=()
else
    command -v sudo >/dev/null 2>&1 ||
        {
            printf 'bootstrap lifecycle test requires root or sudo\n' >&2
            exit 1
        }
    SUDO=(sudo --non-interactive)
fi
readonly -a SUDO

WORKSPACE="$(mktemp -d)"
readonly WORKSPACE
MOCK_PID=""
HOSTS_BACKUP="${WORKSPACE}/hosts"
MOCK_LOG="${WORKSPACE}/https-mock.log"
TOKEN_SOURCE="/root/cirrusync-integration-token.${BASHPID}"
TOOLCHAIN_ROOT=""
TOOLCHAIN_CARGO_LINK_ATTEMPTED=false
TOOLCHAIN_RUSTC_LINK_ATTEMPTED=false
LOCAL_BIN_ORIGINAL_MODE=""
LOCAL_BIN_NORMALIZED_MODE=""
LOCAL_BIN_MODE_CHANGE_ATTEMPTED=false
LOCAL_SRC_ORIGINAL_MODE=""
LOCAL_SRC_NORMALIZED_MODE=""
LOCAL_SRC_MODE_CHANGE_ATTEMPTED=false
CA_INSTALLED=false
HOSTS_CHANGED=false

fail() {
    printf 'bootstrap lifecycle test failed: %s\n' "$*" >&2
    return 1
}

fail_with_log() {
    local message="$1"
    local log_path="$2"

    if [[ -f "${log_path}" ]]; then
        printf '%s\n' '--- captured installer output ---' >&2
        tail -n 200 "${log_path}" >&2
    fi
    fail "${message}"
}

restore_toolchain_entry() {
    local link_path="$1"
    local expected_target="$2"
    local link_attempted="$3"

    if [[ "${link_attempted}" != true ]]; then
        return 0
    fi
    if [[ ! -L "${link_path}" ||
        "$(readlink -- "${link_path}" 2>/dev/null)" != \
        "${expected_target}" ]]; then
        fail "temporary toolchain link changed unexpectedly: ${link_path}"
        return 1
    fi
    "${SUDO[@]}" rm -f -- "${link_path}"
}

normalize_managed_parent_directory() {
    local path="$1"
    local component=""
    local mode=""
    local normalized_mode=""
    local parent_mode=""
    local uid=""

    [[ "${path}" == /usr/local/bin || "${path}" == /usr/local/src ]] ||
        {
            fail "refusing to normalize an unexpected directory: ${path}"
            return 1
        }
    for component in /usr /usr/local "${path}"; do
        [[ -d "${component}" && ! -L "${component}" ]] ||
            {
                fail "managed parent component is not a real directory: ${component}"
                return 1
            }
        uid="$(stat -c '%u' -- "${component}")" || return 1
        [[ "${uid}" == 0 ]] ||
            {
                fail "managed parent component is not owned by root: ${component}"
                return 1
            }
    done

    parent_mode="$(stat -c '%a' -- "${path%/*}")" || return 1
    [[ "${parent_mode}" =~ ^[0-7]{3,4}$ ]] ||
        {
            fail "managed parent has an invalid mode: ${path%/*}"
            return 1
        }
    (( (8#${parent_mode} & 0022) == 0 )) ||
        {
            fail "managed parent is writable by group or other users: ${path%/*}"
            return 1
        }

    mode="$(stat -c '%a' -- "${path}")" || return 1
    [[ "${mode}" =~ ^[0-7]{3,4}$ ]] ||
        {
            fail "managed directory has an invalid mode: ${path}"
            return 1
        }
    printf -v normalized_mode '%o' "$((8#${mode} & 07755))"
    [[ "${normalized_mode}" != "${mode}" ]] || return 0

    case "${path}" in
        /usr/local/bin)
            LOCAL_BIN_ORIGINAL_MODE="${mode}"
            LOCAL_BIN_NORMALIZED_MODE="${normalized_mode}"
            LOCAL_BIN_MODE_CHANGE_ATTEMPTED=true
            ;;
        /usr/local/src)
            LOCAL_SRC_ORIGINAL_MODE="${mode}"
            LOCAL_SRC_NORMALIZED_MODE="${normalized_mode}"
            LOCAL_SRC_MODE_CHANGE_ATTEMPTED=true
            ;;
    esac
    "${SUDO[@]}" chmod "${normalized_mode}" -- "${path}"

    [[ -d "${path}" && ! -L "${path}" &&
        "$(stat -c '%u' -- "${path}")" == 0 &&
        "$(stat -c '%a' -- "${path}")" == "${normalized_mode}" ]] ||
        {
            fail "could not safely normalize managed directory: ${path}"
            return 1
        }
}

restore_managed_parent_mode() {
    local path="$1"
    local original_mode="$2"
    local normalized_mode="$3"
    local change_attempted="$4"
    local current_mode=""

    [[ "${change_attempted}" == true ]] || return 0
    [[ -d "${path}" && ! -L "${path}" ]] ||
        {
            fail "managed directory changed type before mode restoration: ${path}"
            return 1
        }
    [[ "$(stat -c '%u' -- "${path}")" == 0 ]] ||
        {
            fail "managed directory changed owner before mode restoration: ${path}"
            return 1
        }
    current_mode="$(stat -c '%a' -- "${path}")" || return 1
    if [[ "${current_mode}" == "${original_mode}" ]]; then
        return 0
    fi
    [[ "${current_mode}" == "${normalized_mode}" ]] ||
        {
            fail "managed directory mode changed unexpectedly before restoration: ${path}"
            return 1
        }
    "${SUDO[@]}" chmod "${original_mode}" -- "${path}" || return 1
    [[ -d "${path}" && ! -L "${path}" &&
        "$(stat -c '%u' -- "${path}")" == 0 &&
        "$(stat -c '%a' -- "${path}")" == "${original_mode}" ]] ||
        {
            fail "could not restore the original mode on ${path}"
            return 1
        }
}

service_is_confirmed_stopped() {
    local state=""
    local status=""

    if state="$("${SUDO[@]}" systemctl is-active cirrusync.service 2>/dev/null)"; then
        status=0
    else
        status="$?"
    fi
    case "${state}" in
        inactive | failed | not-found | unknown)
            return 0
            ;;
        active | activating | reloading | deactivating)
            fail "service remained in ${state} state during cleanup"
            return 1
            ;;
        *)
            fail "could not confirm that the service stopped (systemctl exit ${status}, state ${state:-<empty>})"
            return 1
            ;;
    esac
}

cleanup() {
    local exit_code="$?"
    local managed_parent_restore_failed=false
    local probe_pid=""
    local toolchain_restore_failed=false

    trap - EXIT
    trap '' HUP INT TERM
    set +e
    if ((exit_code != 0)); then
        "${SUDO[@]}" systemctl --no-pager --full status cirrusync.service >&2
        "${SUDO[@]}" journalctl --no-pager -u cirrusync.service -n 100 >&2
        if [[ -f "${MOCK_LOG}" ]]; then
            printf '%s\n' '--- local HTTPS fixture log ---' >&2
            tail -n 200 "${MOCK_LOG}" >&2
        fi
    fi

    "${SUDO[@]}" systemctl disable --now cirrusync.service >/dev/null 2>&1
    if ! service_is_confirmed_stopped; then
        managed_parent_restore_failed=true
    else
        restore_managed_parent_mode \
            /usr/local/bin \
            "${LOCAL_BIN_ORIGINAL_MODE}" \
            "${LOCAL_BIN_NORMALIZED_MODE}" \
            "${LOCAL_BIN_MODE_CHANGE_ATTEMPTED}" ||
            managed_parent_restore_failed=true
        restore_managed_parent_mode \
            /usr/local/src \
            "${LOCAL_SRC_ORIGINAL_MODE}" \
            "${LOCAL_SRC_NORMALIZED_MODE}" \
            "${LOCAL_SRC_MODE_CHANGE_ATTEMPTED}" ||
            managed_parent_restore_failed=true
    fi
    if [[ "${managed_parent_restore_failed}" == true ]]; then
        exit_code=1
    fi

    if [[ -n "${MOCK_PID}" ]]; then
        "${SUDO[@]}" kill "${MOCK_PID}" >/dev/null 2>&1
        wait "${MOCK_PID}" >/dev/null 2>&1
    fi
    if [[ "${HOSTS_CHANGED}" == true ]]; then
        "${SUDO[@]}" cp -- "${HOSTS_BACKUP}" /etc/hosts
    fi
    if [[ "${CA_INSTALLED}" == true ]]; then
        "${SUDO[@]}" rm -f -- \
            /usr/local/share/ca-certificates/cirrusync-bootstrap-ci.crt
        "${SUDO[@]}" update-ca-certificates --fresh >/dev/null 2>&1
    fi
    if "${SUDO[@]}" test -r "${FSMONITOR_PID_FILE}"; then
        probe_pid="$("${SUDO[@]}" cat "${FSMONITOR_PID_FILE}")"
        if [[ "${probe_pid}" =~ ^[0-9]+$ ]]; then
            "${SUDO[@]}" kill "${probe_pid}" >/dev/null 2>&1
        fi
    fi
    "${SUDO[@]}" rm -f -- \
        "${TOKEN_SOURCE}" \
        "${FSMONITOR_MARKER}" \
        "${FSMONITOR_PID_FILE}" \
        "${FSMONITOR_CONTAMINATION}"
    restore_toolchain_entry \
        "${TOOLCHAIN_CARGO_LINK}" \
        "${TOOLCHAIN_ROOT}/bin/cargo" \
        "${TOOLCHAIN_CARGO_LINK_ATTEMPTED}" ||
        toolchain_restore_failed=true
    restore_toolchain_entry \
        "${TOOLCHAIN_RUSTC_LINK}" \
        "${TOOLCHAIN_ROOT}/bin/rustc" \
        "${TOOLCHAIN_RUSTC_LINK_ATTEMPTED}" ||
        toolchain_restore_failed=true
    if [[ "${toolchain_restore_failed}" == true ]]; then
        printf 'temporary toolchain preserved for recovery at %s\n' \
            "${TOOLCHAIN_ROOT}" >&2
        exit_code=1
    elif [[ "${TOOLCHAIN_ROOT}" == \
        /usr/local/lib/cirrusync-bootstrap-ci.* ]]; then
        "${SUDO[@]}" rm -rf -- "${TOOLCHAIN_ROOT}"
    fi
    rm -rf -- "${WORKSPACE}"
    exit "${exit_code}"
}
trap cleanup EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

require_command() {
    command -v "$1" >/dev/null 2>&1 ||
        fail "required command is missing: $1"
}

wait_for_fixture() {
    local attempt=""

    for ((attempt = 0; attempt < 50; attempt++)); do
        if curl --fail --silent --show-error \
            --cacert "${WORKSPACE}/tls/ca.crt" \
            "https://${FIXTURE_HOST}/health" >/dev/null; then
            return
        fi
        sleep 0.2
    done
    fail "local HTTPS fixture did not become ready"
}

install_trusted_system_toolchain() {
    local cargo_path=""
    local rustc_path=""
    local source_root=""

    cargo_path="$(rustup +stable which cargo)"
    rustc_path="$(rustup +stable which rustc)"
    source_root="$(dirname -- "$(dirname -- "${cargo_path}")")"
    [[ "${rustc_path}" == "${source_root}/bin/rustc" ]] ||
        fail "stable cargo and rustc do not share one toolchain"

    # GitHub-hosted runners make /opt writable. Use a protected system subtree
    # so the fixture satisfies the installer's trusted-ancestor policy.
    TOOLCHAIN_ROOT="$("${SUDO[@]}" mktemp -d \
        /usr/local/lib/cirrusync-bootstrap-ci.XXXXXX)"
    "${SUDO[@]}" cp --archive --reflink=auto -- \
        "${source_root}/." "${TOOLCHAIN_ROOT}/"
    "${SUDO[@]}" chown --recursive root:root "${TOOLCHAIN_ROOT}"
    "${SUDO[@]}" chmod --recursive a+rX,go-w "${TOOLCHAIN_ROOT}"

    [[ ! -e "${TOOLCHAIN_CARGO_LINK}" &&
        ! -L "${TOOLCHAIN_CARGO_LINK}" ]] ||
        fail "refusing to replace existing ${TOOLCHAIN_CARGO_LINK}"
    TOOLCHAIN_CARGO_LINK_ATTEMPTED=true
    "${SUDO[@]}" ln -s \
        "${TOOLCHAIN_ROOT}/bin/cargo" "${TOOLCHAIN_CARGO_LINK}"

    [[ ! -e "${TOOLCHAIN_RUSTC_LINK}" &&
        ! -L "${TOOLCHAIN_RUSTC_LINK}" ]] ||
        fail "refusing to replace existing ${TOOLCHAIN_RUSTC_LINK}"
    TOOLCHAIN_RUSTC_LINK_ATTEMPTED=true
    "${SUDO[@]}" ln -s \
        "${TOOLCHAIN_ROOT}/bin/rustc" "${TOOLCHAIN_RUSTC_LINK}"
}

prepare_repository() {
    local source="${WORKSPACE}/source"
    local bare_repository="${WORKSPACE}/https-root/repo.git"

    cargo vendor --locked "${WORKSPACE}/vendor" >/dev/null
    chmod --recursive a+rX,go-w "${WORKSPACE}/vendor"

    git clone --quiet --no-local -- "${REPOSITORY_ROOT}" "${source}"
    install -d "${source}/.cargo"
    {
        printf '[source.crates-io]\n'
        printf 'replace-with = "cirrusync-ci-vendor"\n\n'
        printf '[source.cirrusync-ci-vendor]\n'
        printf 'directory = "%s"\n' "${WORKSPACE}/vendor"
    } >"${source}/.cargo/config.toml"
    git -C "${source}" add .cargo/config.toml
    git -C "${source}" \
        -c user.name="Cirrusync CI" \
        -c user.email="ci@cirrusync.invalid" \
        commit --quiet -m "Use the lifecycle test's local Cargo vendor"

    install -d "${WORKSPACE}/https-root"
    git init --quiet --bare "${bare_repository}"
    git -C "${source}" push --quiet \
        "${bare_repository}" "HEAD:refs/heads/${INTEGRATION_BRANCH}"
    git --git-dir="${bare_repository}" symbolic-ref \
        HEAD "refs/heads/${INTEGRATION_BRANCH}"
    git --git-dir="${bare_repository}" update-server-info
    chmod --recursive a+rX,go-w "${WORKSPACE}/https-root"
}

prepare_tls_and_hosts() {
    local tls="${WORKSPACE}/tls"

    install -d "${tls}"
    openssl req -x509 -newkey rsa:2048 -nodes -days 1 \
        -subj "/CN=Cirrusync bootstrap CI CA" \
        -keyout "${tls}/ca.key" \
        -out "${tls}/ca.crt" >/dev/null 2>&1
    openssl req -newkey rsa:2048 -nodes \
        -subj "/CN=${FIXTURE_HOST}" \
        -keyout "${tls}/server.key" \
        -out "${tls}/server.csr" >/dev/null 2>&1
    {
        printf 'subjectAltName=DNS:%s,DNS:api.cloudflare.com,' "${FIXTURE_HOST}"
        printf 'DNS:api.ipify.org,DNS:api6.ipify.org\n'
        printf 'extendedKeyUsage=serverAuth\n'
    } >"${tls}/server.ext"
    openssl x509 -req -days 1 \
        -in "${tls}/server.csr" \
        -CA "${tls}/ca.crt" \
        -CAkey "${tls}/ca.key" \
        -CAcreateserial \
        -extfile "${tls}/server.ext" \
        -out "${tls}/server.crt" >/dev/null 2>&1

    "${SUDO[@]}" install -o root -g root -m 0644 \
        "${tls}/ca.crt" \
        /usr/local/share/ca-certificates/cirrusync-bootstrap-ci.crt
    "${SUDO[@]}" update-ca-certificates >/dev/null
    CA_INSTALLED=true

    cp -- /etc/hosts "${HOSTS_BACKUP}"
    printf '127.0.0.1 %s api.cloudflare.com api.ipify.org api6.ipify.org\n' \
        "${FIXTURE_HOST}" |
        "${SUDO[@]}" tee -a /etc/hosts >/dev/null
    HOSTS_CHANGED=true

    "${SUDO[@]}" python3 \
        "${REPOSITORY_ROOT}/tests/bootstrap_https_mock.py" \
        --root "${WORKSPACE}/https-root" \
        --certificate "${tls}/server.crt" \
        --private-key "${tls}/server.key" \
        >"${MOCK_LOG}" 2>&1 &
    MOCK_PID="$!"
    wait_for_fixture
}

write_token_source() {
    printf '%s\n' "${TOKEN_VALUE}" |
        "${SUDO[@]}" tee "${TOKEN_SOURCE}" >/dev/null
    "${SUDO[@]}" chown root:root "${TOKEN_SOURCE}"
    "${SUDO[@]}" chmod 0600 "${TOKEN_SOURCE}"
}

run_failed_fresh_install_and_verify_rollback() {
    local failure_log="${WORKSPACE}/fresh-install-failure.log"

    if "${SUDO[@]}" env \
        CIRRUSYNC_TOKEN_FILE="${TOKEN_SOURCE}" \
        CIRRUSYNC_ZONE="failure.test" \
        CIRRUSYNC_RECORD="host.failure.test" \
        CIRRUSYNC_ENABLE_IPV4=true \
        CIRRUSYNC_ENABLE_IPV6=false \
        CIRRUSYNC_INTERVAL=60 \
        CIRRUSYNC_CREATE=false \
        CIRRUSYNC_PROXIED=false \
        bash "${REPOSITORY_ROOT}/bootstrap.sh" \
        --repo "${REPOSITORY_URL}" \
        --branch "${INTEGRATION_BRANCH}" \
        --non-interactive \
        --skip-tests >"${failure_log}" 2>&1; then
        fail "fresh install unexpectedly passed against a missing zone"
    fi

    grep -Fq 'Rollback completed' "${failure_log}" ||
        fail_with_log \
            "failed fresh install did not report a completed rollback" \
            "${failure_log}"
    for path in \
        "${BINARY_PATH}" \
        "${UNIT_PATH}" \
        "${CONFIG_PATH}" \
        "${TOKEN_PATH}" \
        "${STATE_DIR}" \
        "${SOURCE_DIR}"; do
        "${SUDO[@]}" test ! -e "${path}" ||
            fail "failed fresh install retained ${path}"
    done
    if "${SUDO[@]}" getent passwd cirrusync >/dev/null; then
        fail "failed fresh install retained the new service user"
    fi
    if "${SUDO[@]}" getent group cirrusync >/dev/null; then
        fail "failed fresh install retained the new service group"
    fi
}

run_install() {
    "${SUDO[@]}" env \
        CIRRUSYNC_TOKEN_FILE="${TOKEN_SOURCE}" \
        CIRRUSYNC_ZONE="example.test" \
        CIRRUSYNC_RECORD="host.example.test" \
        CIRRUSYNC_ENABLE_IPV4=true \
        CIRRUSYNC_ENABLE_IPV6=false \
        CIRRUSYNC_INTERVAL=60 \
        CIRRUSYNC_CREATE=false \
        CIRRUSYNC_PROXIED=false \
        bash "${REPOSITORY_ROOT}/bootstrap.sh" \
        --repo "${REPOSITORY_URL}" \
        --branch "${INTEGRATION_BRANCH}" \
        --non-interactive \
        --skip-tests

    "${SUDO[@]}" systemctl is-active --quiet cirrusync.service ||
        fail "fresh install did not leave the service active"
    "${SUDO[@]}" systemctl is-enabled --quiet cirrusync.service ||
        fail "fresh install did not enable the service"
    "${SUDO[@]}" test -x "${BINARY_PATH}" ||
        fail "fresh install did not install the binary"
    "${SUDO[@]}" test -f "${CONFIG_PATH}" ||
        fail "fresh install did not write the configuration"
    "${SUDO[@]}" test -f "${TOKEN_PATH}" ||
        fail "fresh install did not write the token"
    "${SUDO[@]}" test ! -e "${BUILD_STATE_DIR}/cargo/bin/rustup" ||
        fail "installer ignored the trusted system toolchain and installed rustup"
}

run_runtime_file_boundary_checks() {
    local untrusted_config="/etc/cirrusync/untrusted-owner.toml"
    local failure_log="${WORKSPACE}/runtime-boundary-failure.log"

    "${SUDO[@]}" cp -- "${CONFIG_PATH}" "${untrusted_config}"
    "${SUDO[@]}" chown cirrusync-build:cirrusync-build "${untrusted_config}"
    "${SUDO[@]}" chmod 0644 "${untrusted_config}"
    if "${SUDO[@]}" runuser --user cirrusync -- \
        "${BINARY_PATH}" --config "${untrusted_config}" check \
        >"${failure_log}" 2>&1; then
        fail "runtime accepted a configuration owned by the build account"
    fi
    grep -Fq 'neither root nor the current effective UID' "${failure_log}" ||
        fail "runtime did not diagnose third-party configuration ownership"
    "${SUDO[@]}" rm -f -- "${untrusted_config}"

    "${SUDO[@]}" setfacl --modify user:cirrusync-build:r-- "${TOKEN_PATH}"
    if "${SUDO[@]}" runuser --user cirrusync -- \
        "${BINARY_PATH}" --config "${CONFIG_PATH}" check \
        >"${failure_log}" 2>&1; then
        fail "runtime accepted a token with an extended read ACL"
    fi
    grep -Fq 'extended POSIX access ACLs are not accepted' "${failure_log}" ||
        fail "runtime did not diagnose the token access ACL"
    "${SUDO[@]}" setfacl --remove-all "${TOKEN_PATH}"
    "${SUDO[@]}" chown root:cirrusync "${TOKEN_PATH}"
    "${SUDO[@]}" chmod 0640 "${TOKEN_PATH}"

    "${SUDO[@]}" chown root:cirrusync-build "${TOKEN_PATH}"
    if "${SUDO[@]}" "${BINARY_PATH}" --config "${CONFIG_PATH}" check \
        >"${failure_log}" 2>&1; then
        fail "runtime accepted a group-readable token for a different effective group"
    fi
    grep -Fq 'does not match effective GID' "${failure_log}" ||
        fail "runtime did not diagnose the token group mismatch"
    "${SUDO[@]}" chown root:cirrusync "${TOKEN_PATH}"
    "${SUDO[@]}" chmod 0640 "${TOKEN_PATH}"
}

run_prebuild_boundary_checks() {
    local failure_log="${WORKSPACE}/prebuild-boundary-failure.log"
    local token_hash=""

    "${SUDO[@]}" chmod 0644 "${TOKEN_PATH}"
    if "${SUDO[@]}" bash "${REPOSITORY_ROOT}/bootstrap.sh" \
        --repo "${REPOSITORY_URL}" \
        --branch "${INTEGRATION_BRANCH}" \
        --non-interactive \
        --skip-tests \
        --update >"${failure_log}" 2>&1; then
        fail "update built repository code while the token was world-readable"
    fi
    grep -Fq 'readable outside its owner/service boundary' "${failure_log}" ||
        fail "update did not diagnose the unsafe pre-build token"
    "${SUDO[@]}" chown root:cirrusync "${TOKEN_PATH}"
    "${SUDO[@]}" chmod 0640 "${TOKEN_PATH}"
    "${SUDO[@]}" systemctl is-active --quiet cirrusync.service ||
        fail "pre-build token rejection disturbed the active service"

    "${SUDO[@]}" chmod 4755 "${BINARY_PATH}"
    if "${SUDO[@]}" bash "${REPOSITORY_ROOT}/bootstrap.sh" \
        --non-interactive \
        --reconfigure >"${failure_log}" 2>&1; then
        fail "installer accepted a set-ID managed executable"
    fi
    grep -Fq 'must not have set-ID bits' "${failure_log}" ||
        fail "installer did not diagnose the set-ID managed executable"
    "${SUDO[@]}" chmod 0755 "${BINARY_PATH}"

    token_hash="$("${SUDO[@]}" sha256sum "${TOKEN_PATH}")"
    "${SUDO[@]}" setfacl --modify \
        default:user:cirrusync-build:r-x "${CONFIG_DIR}"
    if "${SUDO[@]}" env \
        CIRRUSYNC_TOKEN_FILE="${TOKEN_SOURCE}" \
        CIRRUSYNC_ZONE="example.test" \
        CIRRUSYNC_RECORD="host.example.test" \
        CIRRUSYNC_ENABLE_IPV4=true \
        CIRRUSYNC_ENABLE_IPV6=false \
        CIRRUSYNC_INTERVAL=60 \
        CIRRUSYNC_CREATE=false \
        CIRRUSYNC_PROXIED=false \
        bash "${REPOSITORY_ROOT}/bootstrap.sh" \
        --non-interactive \
        --reconfigure >"${failure_log}" 2>&1; then
        "${SUDO[@]}" setfacl --remove-default "${CONFIG_DIR}"
        fail "installer accepted a configuration directory with a default ACL"
    fi
    "${SUDO[@]}" setfacl --remove-default "${CONFIG_DIR}"
    grep -Fq 'extended or default ACL' "${failure_log}" ||
        fail "installer did not diagnose the configuration directory ACL"
    [[ "$("${SUDO[@]}" sha256sum "${TOKEN_PATH}")" == "${token_hash}" ]] ||
        fail "directory ACL rejection modified the installed token"
    "${SUDO[@]}" systemctl is-active --quiet cirrusync.service ||
        fail "directory ACL rejection did not restore the active service"
}

run_update_and_fsmonitor_check() {
    local build_uid=""
    local observed_uid=""
    local probe_pid=""
    local probe_path="${SOURCE_DIR}/.git/cirrusync-fsmonitor-probe"

    build_uid="$(id -u cirrusync-build)"
    "${SUDO[@]}" install -o root -g root -m 0755 \
        "${REPOSITORY_ROOT}/tests/bootstrap_fsmonitor_probe.sh" \
        "${probe_path}"
    "${SUDO[@]}" git -C "${SOURCE_DIR}" config \
        core.fsmonitor "${probe_path}"
    "${SUDO[@]}" rm -f -- \
        "${FSMONITOR_MARKER}" \
        "${FSMONITOR_PID_FILE}" \
        "${FSMONITOR_CONTAMINATION}"

    "${SUDO[@]}" bash "${REPOSITORY_ROOT}/bootstrap.sh" \
        --repo "${REPOSITORY_URL}" \
        --branch "${INTEGRATION_BRANCH}" \
        --non-interactive \
        --skip-tests \
        --update

    "${SUDO[@]}" test -f "${FSMONITOR_MARKER}" ||
        fail "repository-local core.fsmonitor was not exercised during update"
    observed_uid="$("${SUDO[@]}" stat -c '%u' "${FSMONITOR_MARKER}")"
    [[ "${observed_uid}" == "${build_uid}" && "${observed_uid}" != 0 ]] ||
        fail "core.fsmonitor ran as UID ${observed_uid}, expected build UID ${build_uid}"
    "${SUDO[@]}" test -r "${FSMONITOR_PID_FILE}" ||
        fail "core.fsmonitor did not leave its persistent-process probe"
    probe_pid="$("${SUDO[@]}" cat "${FSMONITOR_PID_FILE}")"
    [[ "${probe_pid}" =~ ^[0-9]+$ ]] ||
        fail "core.fsmonitor wrote an invalid process identifier"
    if "${SUDO[@]}" kill -0 "${probe_pid}" 2>/dev/null; then
        fail "update retained a process launched by the old checkout"
    fi
    "${SUDO[@]}" test ! -e "${FSMONITOR_CONTAMINATION}" ||
        fail "old-checkout process reached the new staged source tree"
    "${SUDO[@]}" systemctl is-active --quiet cirrusync.service ||
        fail "update did not leave the service active"
}

run_failed_reconfigure_and_verify_rollback() {
    local binary_hash=""
    local config_hash=""
    local token_hash=""
    local unit_hash=""
    local failure_log="${WORKSPACE}/reconfigure-failure.log"

    binary_hash="$("${SUDO[@]}" sha256sum "${BINARY_PATH}")"
    config_hash="$("${SUDO[@]}" sha256sum "${CONFIG_PATH}")"
    token_hash="$("${SUDO[@]}" sha256sum "${TOKEN_PATH}")"
    unit_hash="$("${SUDO[@]}" sha256sum "${UNIT_PATH}")"

    if "${SUDO[@]}" env \
        CIRRUSYNC_ZONE="failure.test" \
        CIRRUSYNC_RECORD="host.failure.test" \
        bash "${REPOSITORY_ROOT}/bootstrap.sh" \
        --non-interactive \
        --reconfigure >"${failure_log}" 2>&1; then
        fail "reconfiguration unexpectedly passed against a missing zone"
    fi

    grep -Fq 'Rollback completed' "${failure_log}" ||
        fail_with_log \
            "failed reconfiguration did not report a completed rollback" \
            "${failure_log}"
    [[ "$("${SUDO[@]}" sha256sum "${BINARY_PATH}")" == "${binary_hash}" ]] ||
        fail "rollback changed the installed binary"
    [[ "$("${SUDO[@]}" sha256sum "${CONFIG_PATH}")" == "${config_hash}" ]] ||
        fail "rollback did not restore the configuration"
    [[ "$("${SUDO[@]}" sha256sum "${TOKEN_PATH}")" == "${token_hash}" ]] ||
        fail "rollback did not restore the token"
    [[ "$("${SUDO[@]}" sha256sum "${UNIT_PATH}")" == "${unit_hash}" ]] ||
        fail "rollback did not restore the systemd unit"
    "${SUDO[@]}" systemctl is-active --quiet cirrusync.service ||
        fail "rollback did not restart the last-known-good service"
}

run_noninteractive_uninstall() {
    "${SUDO[@]}" bash "${REPOSITORY_ROOT}/bootstrap.sh" \
        --config "${CONFIG_PATH}" \
        --non-interactive \
        --uninstall

    "${SUDO[@]}" test ! -e "${BINARY_PATH}" ||
        fail "non-interactive uninstall retained the binary"
    "${SUDO[@]}" test ! -e "${UNIT_PATH}" ||
        fail "non-interactive uninstall retained the unit"
    if "${SUDO[@]}" systemctl is-active --quiet cirrusync.service; then
        fail "non-interactive uninstall left the service active"
    fi
    "${SUDO[@]}" test -f "${CONFIG_PATH}" ||
        fail "non-interactive uninstall deleted configuration without consent"
    "${SUDO[@]}" test -f "${TOKEN_PATH}" ||
        fail "non-interactive uninstall deleted the token without consent"
    "${SUDO[@]}" test -d "${SOURCE_DIR}" ||
        fail "non-interactive uninstall deleted managed source without consent"
}

for command in cargo curl git openssl python3 rustup setfacl; do
    require_command "${command}"
done
[[ -d /run/systemd/system ]] ||
    fail "bootstrap lifecycle test requires a systemd-booted disposable host"

normalize_managed_parent_directory /usr/local/bin
normalize_managed_parent_directory /usr/local/src
chmod 0755 "${WORKSPACE}"
prepare_repository
install_trusted_system_toolchain
prepare_tls_and_hosts
write_token_source
run_failed_fresh_install_and_verify_rollback
run_install
run_runtime_file_boundary_checks
run_prebuild_boundary_checks
run_update_and_fsmonitor_check
run_failed_reconfigure_and_verify_rollback
run_noninteractive_uninstall

printf 'bootstrap lifecycle checks passed\n'
