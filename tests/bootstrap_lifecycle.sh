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
readonly TOKEN_VALUE="cfat_cirrusync-integration-token"
readonly ACCOUNT_ID="ABCDEF0123456789ABCDEF0123456789"
readonly CANONICAL_ACCOUNT_ID="abcdef0123456789abcdef0123456789"
readonly CONFIG_DIR="/etc/cirrusync"
readonly CONFIG_PATH="/etc/cirrusync/config.toml"
readonly TOKEN_PATH="/etc/cirrusync/token"
readonly CA_SOURCE_PATH="/usr/local/share/ca-certificates/cirrusync-bootstrap-ci.crt"
readonly CA_HOOKS_DIR="/run/cirrusync-bootstrap-ci-ca-hooks-${BASHPID}"
readonly SOURCE_DIR="/usr/local/src/cirrusync"
readonly BINARY_PATH="/usr/local/bin/cirrusync"
readonly UNIT_PATH="/etc/systemd/system/cirrusync.service"
readonly MOCK_UNIT="cirrusync-bootstrap-ci-${BASHPID}.service"
readonly FSMONITOR_MARKER="/var/tmp/cirrusync-fsmonitor-probe.uid"
readonly FSMONITOR_PID_FILE="/var/tmp/cirrusync-fsmonitor-probe.pid"
readonly FSMONITOR_CONTAMINATION="/var/tmp/cirrusync-fsmonitor-contaminated"
readonly STATE_DIR="/var/lib/cirrusync"
readonly BUILD_STATE_DIR="/var/lib/cirrusync-build"
readonly TOOLCHAIN_CARGO_LINK="/usr/sbin/cargo"
readonly TOOLCHAIN_RUSTC_LINK="/usr/sbin/rustc"
readonly VERSIONING_SCRIPT="${REPOSITORY_ROOT}/scripts/versioning.py"

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
MOCK_UNIT_ATTEMPTED=false
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
CA_HOOKS_DIR_ATTEMPTED=false
HOSTS_CHANGED=false
INITIAL_VERSION=""
UPGRADE_VERSION=""
PYTHON_BIN=""

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

run_root_bounded() {
    local duration="$1"

    shift
    "${SUDO[@]}" timeout \
        --signal=TERM \
        --kill-after=2s \
        "${duration}" \
        "$@"
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
    run_root_bounded 10s rm -f -- "${link_path}"
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
    run_root_bounded 10s chmod "${original_mode}" -- "${path}" || return 1
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

    if state="$(run_root_bounded 10s \
        systemctl is-active cirrusync.service 2>/dev/null)"; then
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

service_is_confirmed_disabled() {
    local enable_state=""
    local load_state=""
    local status=""

    load_state="$(run_root_bounded 10s systemctl show \
        --property=LoadState --value cirrusync.service 2>/dev/null)" ||
        return 1
    if enable_state="$(run_root_bounded 10s \
        systemctl is-enabled cirrusync.service 2>/dev/null)"; then
        status=0
    else
        status="$?"
    fi
    case "${load_state}:${enable_state}" in
        loaded:disabled | not-found:disabled | not-found:not-found)
            return 0
            ;;
        not-found:)
            [[ "${status}" == 1 || "${status}" == 4 ]] && return 0
            ;;
    esac
    fail "service remained ${enable_state:-<empty>} with load state ${load_state:-<empty>} during cleanup (systemctl exit ${status})"
    return 1
}

stop_and_disable_installed_service() {
    if ! run_root_bounded 30s systemctl stop \
        cirrusync.service >/dev/null 2>&1; then
        run_root_bounded 5s systemctl kill \
            --kill-who=all \
            --signal=KILL \
            cirrusync.service >/dev/null 2>&1 || true
        run_root_bounded 5s systemctl stop \
            cirrusync.service >/dev/null 2>&1 || true
    fi
    run_root_bounded 10s systemctl disable \
        cirrusync.service >/dev/null 2>&1 || true
    run_root_bounded 10s systemctl disable --runtime \
        cirrusync.service >/dev/null 2>&1 || true

    service_is_confirmed_stopped &&
        service_is_confirmed_disabled
}

stop_mock_server() {
    local active_state=""
    local load_state=""

    [[ "${MOCK_UNIT_ATTEMPTED}" == true ]] || return 0
    if ! run_root_bounded 12s systemctl stop "${MOCK_UNIT}" \
        >/dev/null 2>&1; then
        run_root_bounded 5s systemctl kill \
            --kill-who=all \
            --signal=KILL \
            "${MOCK_UNIT}" >/dev/null 2>&1 || true
        run_root_bounded 5s systemctl stop "${MOCK_UNIT}" \
            >/dev/null 2>&1 || true
    fi

    load_state="$(run_root_bounded 5s systemctl show \
        --property=LoadState --value "${MOCK_UNIT}" 2>/dev/null)" ||
        return 1
    if [[ "${load_state}" == not-found ]]; then
        return 0
    fi
    [[ "${load_state}" == loaded ]] || return 1
    active_state="$(run_root_bounded 5s systemctl show \
        --property=ActiveState --value "${MOCK_UNIT}" 2>/dev/null)" ||
        return 1
    case "${active_state}" in
        inactive | failed)
            run_root_bounded 5s systemctl reset-failed \
                "${MOCK_UNIT}" >/dev/null 2>&1 || true
            return 0
            ;;
        *)
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
        run_root_bounded 10s systemctl \
            --no-pager --full status cirrusync.service >&2
        run_root_bounded 10s journalctl \
            --no-pager -u cirrusync.service -n 100 >&2
        if [[ -f "${MOCK_LOG}" ]]; then
            printf '%s\n' '--- local HTTPS fixture log ---' >&2
            tail -n 200 "${MOCK_LOG}" >&2
        fi
    fi

    printf '[cirrusync-ci] cleanup: stopping installed service\n' >&2
    if ! stop_and_disable_installed_service; then
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

    printf '[cirrusync-ci] cleanup: stopping HTTPS fixture\n' >&2
    stop_mock_server ||
        {
            printf 'could not stop the HTTPS fixture\n' >&2
            exit_code=1
        }
    if [[ "${HOSTS_CHANGED}" == true ]]; then
        printf '[cirrusync-ci] cleanup: restoring /etc/hosts\n' >&2
        run_root_bounded 10s cp -- "${HOSTS_BACKUP}" /etc/hosts ||
            exit_code=1
    fi
    if [[ "${CA_INSTALLED}" == true ]]; then
        printf '[cirrusync-ci] cleanup: restoring system trust store\n' >&2
        run_root_bounded 10s rm -f -- \
            "${CA_SOURCE_PATH}" \
            /etc/ssl/certs/cirrusync-bootstrap-ci.pem ||
            exit_code=1
        run_root_bounded 10s find /etc/ssl/certs \
            -maxdepth 1 \
            -type l \
            -lname 'cirrusync-bootstrap-ci.pem' \
            -delete ||
            exit_code=1
        # Incremental mode rewrites the bundle without forcing every
        # distribution-specific trust store to import fixture-only state.
        run_root_bounded 60s update-ca-certificates \
            --hooksdir "${CA_HOOKS_DIR}" >/dev/null 2>&1 ||
            exit_code=1
        if [[ -e "${CA_SOURCE_PATH}" || -L "${CA_SOURCE_PATH}" ]]; then
            printf 'temporary CA source remained installed\n' >&2
            exit_code=1
        fi
    fi
    if [[ "${CA_HOOKS_DIR_ATTEMPTED}" == true ]]; then
        run_root_bounded 10s rm -rf -- "${CA_HOOKS_DIR}" ||
            exit_code=1
        if [[ -e "${CA_HOOKS_DIR}" || -L "${CA_HOOKS_DIR}" ]]; then
            printf 'temporary CA hooks directory remained installed\n' >&2
            exit_code=1
        fi
    fi
    if run_root_bounded 5s test -r "${FSMONITOR_PID_FILE}"; then
        probe_pid="$(run_root_bounded 5s cat "${FSMONITOR_PID_FILE}")"
        if [[ "${probe_pid}" =~ ^[0-9]+$ ]]; then
            run_root_bounded 5s kill "${probe_pid}" >/dev/null 2>&1 ||
                true
        fi
    fi
    run_root_bounded 10s rm -f -- \
        "${TOKEN_SOURCE}" \
        "${FSMONITOR_MARKER}" \
        "${FSMONITOR_PID_FILE}" \
        "${FSMONITOR_CONTAMINATION}" ||
        exit_code=1
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
        printf '[cirrusync-ci] cleanup: removing toolchain fixture\n' >&2
        run_root_bounded 120s rm -rf -- "${TOOLCHAIN_ROOT}" ||
            exit_code=1
    fi
    printf '[cirrusync-ci] cleanup: removing workspace\n' >&2
    run_root_bounded 120s rm -rf -- "${WORKSPACE}" ||
        exit_code=1
    printf '[cirrusync-ci] cleanup: complete\n' >&2
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

advance_fixture_repository_version() {
    local source="${WORKSPACE}/source"
    local bare_repository="${WORKSPACE}/https-root/repo.git"

    [[ -n "${INITIAL_VERSION}" && -z "${UPGRADE_VERSION}" ]] ||
        fail "fixture version transition was requested in an invalid state"
    UPGRADE_VERSION="$("${PYTHON_BIN}" "${VERSIONING_SCRIPT}" \
        --root "${source}" bump patch)" ||
        fail "could not advance the lifecycle fixture package version"
    [[ "${UPGRADE_VERSION}" =~ ^(0|[1-9][0-9]{0,8})\.(0|[1-9][0-9]{0,8})\.(0|[1-9][0-9]{0,8})$ &&
        "${UPGRADE_VERSION}" != "${INITIAL_VERSION}" ]] ||
        fail "fixture version helper returned an invalid upgrade version: ${UPGRADE_VERSION}"
    [[ "$("${PYTHON_BIN}" "${VERSIONING_SCRIPT}" \
        --root "${source}" get)" == "${UPGRADE_VERSION}" ]] ||
        fail "fixture manifest did not advance to ${UPGRADE_VERSION}"

    git -C "${source}" add Cargo.toml Cargo.lock
    git -C "${source}" \
        -c user.name="Cirrusync CI" \
        -c user.email="ci@cirrusync.invalid" \
        commit --quiet -m "Advance lifecycle fixture to ${UPGRADE_VERSION}"
    git -C "${source}" push --quiet \
        "${bare_repository}" "HEAD:refs/heads/${INTEGRATION_BRANCH}"
    git --git-dir="${bare_repository}" update-server-info
    chmod --recursive a+rX,go-w "${WORKSPACE}/https-root"
}

publish_fixture_downgrade() {
    local source="${WORKSPACE}/source"
    local bare_repository="${WORKSPACE}/https-root/repo.git"

    [[ -n "${INITIAL_VERSION}" && -n "${UPGRADE_VERSION}" ]] ||
        fail "fixture downgrade was requested before the upgrade version existed"
    git -C "${source}" checkout --quiet HEAD^ -- Cargo.toml Cargo.lock
    [[ "$("${PYTHON_BIN}" "${VERSIONING_SCRIPT}" \
        --root "${source}" get)" == "${INITIAL_VERSION}" ]] ||
        fail "fixture downgrade did not restore ${INITIAL_VERSION}"
    git -C "${source}" \
        -c user.name="Cirrusync CI" \
        -c user.email="ci@cirrusync.invalid" \
        commit --quiet -m "Publish lifecycle downgrade candidate"
    git -C "${source}" push --quiet \
        "${bare_repository}" "HEAD:refs/heads/${INTEGRATION_BRANCH}"
    git --git-dir="${bare_repository}" update-server-info
    chmod --recursive a+rX,go-w "${WORKSPACE}/https-root"
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
    INITIAL_VERSION="$("${PYTHON_BIN}" "${VERSIONING_SCRIPT}" \
        --root "${source}" get)" ||
        fail "could not read the lifecycle fixture package version"
    [[ "${INITIAL_VERSION}" =~ ^(0|[1-9][0-9]{0,8})\.(0|[1-9][0-9]{0,8})\.(0|[1-9][0-9]{0,8})$ ]] ||
        fail "fixture version helper returned an invalid package version: ${INITIAL_VERSION}"

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

    [[ ! -e "${CA_HOOKS_DIR}" && ! -L "${CA_HOOKS_DIR}" ]] ||
        fail "temporary CA hooks path already exists: ${CA_HOOKS_DIR}"
    CA_HOOKS_DIR_ATTEMPTED=true
    run_root_bounded 10s install -d -o root -g root -m 0700 \
        "${CA_HOOKS_DIR}"
    [[ ! -e "${CA_SOURCE_PATH}" && ! -L "${CA_SOURCE_PATH}" &&
        ! -e /etc/ssl/certs/cirrusync-bootstrap-ci.pem &&
        ! -L /etc/ssl/certs/cirrusync-bootstrap-ci.pem ]] ||
        fail "refusing to replace pre-existing CA fixture state"
    CA_INSTALLED=true
    run_root_bounded 10s install -o root -g root -m 0644 \
        "${tls}/ca.crt" \
        "${CA_SOURCE_PATH}"
    run_root_bounded 60s update-ca-certificates \
        --hooksdir "${CA_HOOKS_DIR}" >/dev/null

    run_root_bounded 10s cp -- /etc/hosts "${HOSTS_BACKUP}"
    HOSTS_CHANGED=true
    printf '127.0.0.1 %s api.cloudflare.com api.ipify.org api6.ipify.org\n' \
        "${FIXTURE_HOST}" |
        run_root_bounded 10s tee -a /etc/hosts >/dev/null

    # A transient unit gives the privileged fixture its own cgroup. Capturing
    # $! from `sudo python3 ... &` tracks sudo's supervisor on older releases,
    # so signal forwarding and wait(1) are not reliable cleanup primitives.
    MOCK_UNIT_ATTEMPTED=true
    run_root_bounded 15s systemd-run \
        --quiet \
        --collect \
        --unit="${MOCK_UNIT}" \
        --property=Type=exec \
        --property=KillMode=control-group \
        --property=TimeoutStopSec=5s \
        --property=RuntimeMaxSec=30min \
        --property="StandardOutput=append:${MOCK_LOG}" \
        --property="StandardError=append:${MOCK_LOG}" \
        -- \
        "${PYTHON_BIN}" \
        "${REPOSITORY_ROOT}/tests/bootstrap_https_mock.py" \
        --root "${WORKSPACE}/https-root" \
        --certificate "${tls}/server.crt" \
        --private-key "${tls}/server.key"
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
        CIRRUSYNC_ACCOUNT_ID="${ACCOUNT_ID}" \
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
    local installed_version=""

    "${SUDO[@]}" env \
        CIRRUSYNC_TOKEN_FILE="${TOKEN_SOURCE}" \
        CIRRUSYNC_ZONE="example.test" \
        CIRRUSYNC_ACCOUNT_ID="${ACCOUNT_ID}" \
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
    installed_version="$("${SUDO[@]}" runuser --user cirrusync -- \
        "${BINARY_PATH}" --version)" ||
        fail "freshly installed binary did not report its version"
    [[ "${installed_version}" == "cirrusync ${INITIAL_VERSION}" ]] ||
        fail "fresh install reported ${installed_version}, expected cirrusync ${INITIAL_VERSION}"
    "${SUDO[@]}" test -f "${CONFIG_PATH}" ||
        fail "fresh install did not write the configuration"
    "${SUDO[@]}" grep -Fqx \
        "account_id = \"${CANONICAL_ACCOUNT_ID}\"" \
        "${CONFIG_PATH}" ||
        fail "fresh install did not write the canonical Cloudflare account ID"
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
    local reconfigure_succeeded=false
    local service_active_during_rejection=false
    local service_invocation_after=""
    local service_invocation_before=""
    local service_pid_after=""
    local service_pid_before=""
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
    service_pid_before="$("${SUDO[@]}" systemctl show \
        --property=MainPID --value cirrusync.service)" ||
        fail "could not read the active service PID before the ACL check"
    [[ "${service_pid_before}" =~ ^[1-9][0-9]*$ ]] ||
        fail "the service had no main process before the ACL check"
    service_invocation_before="$("${SUDO[@]}" systemctl show \
        --property=InvocationID --value cirrusync.service)" ||
        fail "could not read the service invocation before the ACL check"
    [[ "${service_invocation_before}" =~ ^[0-9a-fA-F]{32}$ ]] ||
        fail "the service had no invocation ID before the ACL check"
    "${SUDO[@]}" setfacl --modify \
        default:user:cirrusync-build:r-x "${CONFIG_DIR}"
    if "${SUDO[@]}" env \
        CIRRUSYNC_TOKEN_FILE="${TOKEN_SOURCE}" \
        CIRRUSYNC_ZONE="example.test" \
        CIRRUSYNC_ACCOUNT_ID="${ACCOUNT_ID}" \
        CIRRUSYNC_RECORD="host.example.test" \
        CIRRUSYNC_ENABLE_IPV4=true \
        CIRRUSYNC_ENABLE_IPV6=false \
        CIRRUSYNC_INTERVAL=60 \
        CIRRUSYNC_CREATE=false \
        CIRRUSYNC_PROXIED=false \
        bash "${REPOSITORY_ROOT}/bootstrap.sh" \
        --non-interactive \
        --reconfigure >"${failure_log}" 2>&1; then
        reconfigure_succeeded=true
    fi
    if "${SUDO[@]}" systemctl is-active --quiet cirrusync.service; then
        service_active_during_rejection=true
        if service_pid_after="$("${SUDO[@]}" systemctl show \
            --property=MainPID --value cirrusync.service 2>/dev/null)" &&
            service_invocation_after="$("${SUDO[@]}" systemctl show \
                --property=InvocationID --value \
                cirrusync.service 2>/dev/null)"; then
            :
        else
            service_active_during_rejection=false
        fi
    fi
    "${SUDO[@]}" setfacl --remove-default "${CONFIG_DIR}"
    [[ "${reconfigure_succeeded}" == false ]] ||
        fail "installer accepted a configuration directory with a default ACL"
    grep -Fq 'extended or default ACL' "${failure_log}" ||
        fail "installer did not diagnose the configuration directory ACL"
    [[ "$("${SUDO[@]}" sha256sum "${TOKEN_PATH}")" == "${token_hash}" ]] ||
        fail "directory ACL rejection modified the installed token"
    [[ "${service_active_during_rejection}" == true ]] ||
        fail "directory ACL rejection stopped the active service"
    [[ "${service_pid_after}" == "${service_pid_before}" &&
        "${service_invocation_after}" == "${service_invocation_before}" ]] ||
        fail "directory ACL rejection restarted the active service"
}

run_upgrade_and_fsmonitor_check() {
    local binary_hash_after=""
    local binary_hash_before=""
    local build_uid=""
    local first_upgrade_log="${WORKSPACE}/first-upgrade.log"
    local downgrade_log="${WORKSPACE}/downgrade-rejection.log"
    local installed_version=""
    local invocation_after=""
    local invocation_before=""
    local observed_uid=""
    local pid_after=""
    local pid_before=""
    local probe_pid=""
    local probe_path="${SOURCE_DIR}/.git/cirrusync-fsmonitor-probe"
    local repair_log="${WORKSPACE}/incomplete-upgrade-repair.log"
    local runtime_repair_log="${WORKSPACE}/runtime-state-repair.log"
    local second_upgrade_log="${WORKSPACE}/second-upgrade.log"
    local service_gid=""

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

    advance_fixture_repository_version

    if ! "${SUDO[@]}" bash "${REPOSITORY_ROOT}/bootstrap.sh" \
        --repo "${REPOSITORY_URL}" \
        --branch "${INTEGRATION_BRANCH}" \
        --non-interactive \
        --skip-tests \
        --upgrade >"${first_upgrade_log}" 2>&1; then
        fail_with_log "version-aware upgrade failed" "${first_upgrade_log}"
    fi

    "${SUDO[@]}" test -f "${FSMONITOR_MARKER}" ||
        fail "repository-local core.fsmonitor was not exercised during upgrade"
    observed_uid="$("${SUDO[@]}" stat -c '%u' "${FSMONITOR_MARKER}")"
    [[ "${observed_uid}" == "${build_uid}" && "${observed_uid}" != 0 ]] ||
        fail "core.fsmonitor ran as UID ${observed_uid}, expected build UID ${build_uid}"
    "${SUDO[@]}" test -r "${FSMONITOR_PID_FILE}" ||
        fail "core.fsmonitor did not leave its persistent-process probe"
    probe_pid="$("${SUDO[@]}" cat "${FSMONITOR_PID_FILE}")"
    [[ "${probe_pid}" =~ ^[0-9]+$ ]] ||
        fail "core.fsmonitor wrote an invalid process identifier"
    if "${SUDO[@]}" kill -0 "${probe_pid}" 2>/dev/null; then
        fail "upgrade retained a process launched by the old checkout"
    fi
    "${SUDO[@]}" test ! -e "${FSMONITOR_CONTAMINATION}" ||
        fail "old-checkout process reached the upgraded staged source tree"
    "${SUDO[@]}" systemctl is-active --quiet cirrusync.service ||
        fail "upgrade did not leave the service active"
    installed_version="$("${SUDO[@]}" runuser --user cirrusync -- \
        "${BINARY_PATH}" --version)" ||
        fail "upgraded binary did not report its version"
    [[ "${installed_version}" == "cirrusync ${UPGRADE_VERSION}" ]] ||
        fail "upgrade reported ${installed_version}, expected cirrusync ${UPGRADE_VERSION}"

    binary_hash_before="$("${SUDO[@]}" sha256sum "${BINARY_PATH}")"
    pid_before="$("${SUDO[@]}" systemctl show \
        --property=MainPID --value cirrusync.service)" ||
        fail "could not read the service PID before the no-op upgrade"
    invocation_before="$("${SUDO[@]}" systemctl show \
        --property=InvocationID --value cirrusync.service)" ||
        fail "could not read the service invocation before the no-op upgrade"
    [[ "${pid_before}" =~ ^[1-9][0-9]*$ &&
        "${invocation_before}" =~ ^[0-9a-fA-F]{32}$ ]] ||
        fail "service identity was invalid before the no-op upgrade"

    if ! "${SUDO[@]}" bash "${REPOSITORY_ROOT}/bootstrap.sh" \
        --repo "${REPOSITORY_URL}" \
        --branch "${INTEGRATION_BRANCH}" \
        --non-interactive \
        --skip-tests \
        --upgrade >"${second_upgrade_log}" 2>&1; then
        fail_with_log "identical version upgrade failed" "${second_upgrade_log}"
    fi

    grep -Fq "Cirrusync ${UPGRADE_VERSION} is already current" \
        "${second_upgrade_log}" ||
        fail_with_log "identical upgrade did not report its no-op" \
            "${second_upgrade_log}"
    grep -Fq "No Cirrusync files were" "${second_upgrade_log}" ||
        fail_with_log "identical upgrade did not describe preserved state" \
            "${second_upgrade_log}"
    binary_hash_after="$("${SUDO[@]}" sha256sum "${BINARY_PATH}")"
    pid_after="$("${SUDO[@]}" systemctl show \
        --property=MainPID --value cirrusync.service)" ||
        fail "could not read the service PID after the no-op upgrade"
    invocation_after="$("${SUDO[@]}" systemctl show \
        --property=InvocationID --value cirrusync.service)" ||
        fail "could not read the service invocation after the no-op upgrade"
    [[ "${binary_hash_after}" == "${binary_hash_before}" ]] ||
        fail "identical version upgrade replaced the installed binary"
    [[ "${pid_after}" == "${pid_before}" &&
        "${invocation_after}" == "${invocation_before}" ]] ||
        fail "identical version upgrade restarted the service"
    "${SUDO[@]}" systemctl is-active --quiet cirrusync.service ||
        fail "identical version upgrade did not leave the service active"

    service_gid="$("${SUDO[@]}" getent group cirrusync | cut -d: -f3)"
    [[ "${service_gid}" =~ ^[1-9][0-9]*$ ]] ||
        fail "could not resolve the service group before runtime repair"
    "${SUDO[@]}" chown root:root "${CONFIG_PATH}" "${TOKEN_PATH}"
    "${SUDO[@]}" chmod 0600 "${CONFIG_PATH}" "${TOKEN_PATH}"

    if ! "${SUDO[@]}" bash "${REPOSITORY_ROOT}/bootstrap.sh" \
        --repo "${REPOSITORY_URL}" \
        --branch "${INTEGRATION_BRANCH}" \
        --non-interactive \
        --skip-tests \
        --upgrade >"${runtime_repair_log}" 2>&1; then
        fail_with_log "runtime-state repair failed" "${runtime_repair_log}"
    fi
    grep -Fq \
        "Installed runtime state needs repair; rebuilding the current release" \
        "${runtime_repair_log}" ||
        fail_with_log "permission drift bypassed runtime-state inspection" \
            "${runtime_repair_log}"
    grep -Fq "Repairing the incomplete Cirrusync ${UPGRADE_VERSION} installation" \
        "${runtime_repair_log}" ||
        fail_with_log "runtime drift did not enter repair mode" \
            "${runtime_repair_log}"
    [[ "$("${SUDO[@]}" stat -c '%u:%g:%a' "${CONFIG_PATH}")" == \
        "0:${service_gid}:640" ]] ||
        fail "runtime repair did not normalize configuration permissions"
    [[ "$("${SUDO[@]}" stat -c '%u:%g:%a' "${TOKEN_PATH}")" == \
        "0:${service_gid}:640" ]] ||
        fail "runtime repair did not normalize token permissions"
    "${SUDO[@]}" systemctl is-active --quiet cirrusync.service ||
        fail "runtime repair did not leave the service active"

    "${SUDO[@]}" rm -f -- "${TOKEN_PATH}" "${UNIT_PATH}"
    if ! "${SUDO[@]}" bash "${REPOSITORY_ROOT}/bootstrap.sh" \
        --repo "${REPOSITORY_URL}" \
        --branch "${INTEGRATION_BRANCH}" \
        --config "${CONFIG_PATH}" \
        --token-file "${TOKEN_SOURCE}" \
        --non-interactive \
        --skip-tests \
        --upgrade >"${repair_log}" 2>&1; then
        fail_with_log "incomplete equal-version repair failed" "${repair_log}"
    fi
    grep -Fq "Repairing the incomplete Cirrusync ${UPGRADE_VERSION} installation" \
        "${repair_log}" ||
        fail_with_log "incomplete upgrade did not enter repair mode" \
            "${repair_log}"
    "${SUDO[@]}" test -f "${TOKEN_PATH}" ||
        fail "incomplete upgrade did not restore the token"
    "${SUDO[@]}" test -f "${UNIT_PATH}" ||
        fail "incomplete upgrade did not restore the systemd unit"
    "${SUDO[@]}" systemctl is-active --quiet cirrusync.service ||
        fail "incomplete upgrade repair did not leave the service active"

    binary_hash_before="$("${SUDO[@]}" sha256sum "${BINARY_PATH}")"
    pid_before="$("${SUDO[@]}" systemctl show \
        --property=MainPID --value cirrusync.service)"
    invocation_before="$("${SUDO[@]}" systemctl show \
        --property=InvocationID --value cirrusync.service)"
    publish_fixture_downgrade
    if "${SUDO[@]}" bash "${REPOSITORY_ROOT}/bootstrap.sh" \
        --repo "${REPOSITORY_URL}" \
        --branch "${INTEGRATION_BRANCH}" \
        --non-interactive \
        --skip-tests \
        --upgrade >"${downgrade_log}" 2>&1; then
        fail "downgrade candidate was unexpectedly installed"
    fi
    grep -Fq \
        "refusing to downgrade Cirrusync ${UPGRADE_VERSION} to ${INITIAL_VERSION}" \
        "${downgrade_log}" ||
        fail_with_log "downgrade rejection was not diagnosed" "${downgrade_log}"
    [[ "$("${SUDO[@]}" sha256sum "${BINARY_PATH}")" == \
        "${binary_hash_before}" ]] ||
        fail "downgrade rejection changed the installed binary"
    [[ "$("${SUDO[@]}" "${PYTHON_BIN}" "${VERSIONING_SCRIPT}" \
        --root "${SOURCE_DIR}" get)" == "${UPGRADE_VERSION}" ]] ||
        fail "downgrade rejection changed the managed source"
    pid_after="$("${SUDO[@]}" systemctl show \
        --property=MainPID --value cirrusync.service)"
    invocation_after="$("${SUDO[@]}" systemctl show \
        --property=InvocationID --value cirrusync.service)"
    [[ "${pid_after}" == "${pid_before}" &&
        "${invocation_after}" == "${invocation_before}" ]] ||
        fail "downgrade rejection restarted the service"
    "${SUDO[@]}" systemctl is-active --quiet cirrusync.service ||
        fail "downgrade rejection did not leave the service active"
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
        CIRRUSYNC_ACCOUNT_ID="${ACCOUNT_ID}" \
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

for command in cargo curl find git openssl prlimit python3 rustup setfacl \
    systemctl systemd-run timeout; do
    require_command "${command}"
done
PYTHON_BIN="$(command -v python3)"
readonly PYTHON_BIN
[[ -f "${VERSIONING_SCRIPT}" && ! -L "${VERSIONING_SCRIPT}" ]] ||
    fail "version policy helper is missing or unsafe: ${VERSIONING_SCRIPT}"
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
run_upgrade_and_fsmonitor_check
run_failed_reconfigure_and_verify_rollback
run_noninteractive_uninstall

printf 'bootstrap lifecycle checks passed\n'
