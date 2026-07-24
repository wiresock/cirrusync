#!/usr/bin/env bash
#
# Install, update, reconfigure, or uninstall Cirrusync on Debian and Ubuntu.
# Review this script before running it as root.

set -Eeuo pipefail
IFS=$'\n\t'
umask 0022
readonly TRUSTED_PATH="/usr/sbin:/usr/bin:/sbin:/bin:/usr/local/sbin:/usr/local/bin"
export PATH="${TRUSTED_PATH}"

BOOTSTRAP_IS_SOURCED=false
if [[ -n "${BASH_SOURCE[0]:-}" && "${BASH_SOURCE[0]}" != "$0" ]]; then
    BOOTSTRAP_IS_SOURCED=true
fi
readonly BOOTSTRAP_IS_SOURCED

BOOTSTRAP_ROOT=""
if [[ "${BOOTSTRAP_IS_SOURCED}" == true ]]; then
    BOOTSTRAP_ROOT="${CIRRUSYNC_BOOTSTRAP_TEST_ROOT:-}"
    [[ -z "${BOOTSTRAP_ROOT}" ||
        ( "${BOOTSTRAP_ROOT}" == /* && "${BOOTSTRAP_ROOT}" != / &&
            "${BOOTSTRAP_ROOT}" != */ ) ]] ||
        {
            printf '[cirrusync] ERROR: CIRRUSYNC_BOOTSTRAP_TEST_ROOT must be an absolute non-root path\n' >&2
            return 1 2>/dev/null || exit 1
        }
fi
readonly BOOTSTRAP_ROOT

readonly SERVICE_NAME="cirrusync.service"
readonly SERVICE_USER="cirrusync"
readonly SERVICE_GROUP="cirrusync"
readonly BUILD_USER="cirrusync-build"
readonly BUILD_GROUP="cirrusync-build"
readonly BINARY_PATH="${BOOTSTRAP_ROOT}/usr/local/bin/cirrusync"
readonly SOURCE_DIR="${BOOTSTRAP_ROOT}/usr/local/src/cirrusync"
readonly STATE_DIR="${BOOTSTRAP_ROOT}/var/lib/cirrusync"
readonly BUILD_STATE_DIR="${BOOTSTRAP_ROOT}/var/lib/cirrusync-build"
readonly UNIT_PATH="${BOOTSTRAP_ROOT}/etc/systemd/system/cirrusync.service"
readonly UNIT_OVERRIDE_DIR="${BOOTSTRAP_ROOT}/etc/systemd/system/cirrusync.service.d"
readonly UNIT_OVERRIDE_PATH="${UNIT_OVERRIDE_DIR}/10-config-path.conf"
readonly CONFIG_ROOT="${BOOTSTRAP_ROOT}/etc/cirrusync"
readonly DEFAULT_CONFIG_PATH="${CONFIG_ROOT}/config.toml"
readonly DEFAULT_REPOSITORY="https://github.com/wiresock/cirrusync.git"
readonly INSTALL_LOCK_PATH="${BOOTSTRAP_ROOT}/run/cirrusync-bootstrap.lock"
# The bootstrap may build an arbitrary --repo as an unprivileged user, but it
# must never install repository-controlled service privileges as root.
readonly SYSTEMD_UNIT_SHA256="9cdb70fe14082a829cfe06633d4a330b4c7e445bebe218fce947dd19d2b3b65a"

REPO_URL="${DEFAULT_REPOSITORY}"
BRANCH="main"
CONFIG_PATH="${DEFAULT_CONFIG_PATH}"
TOKEN_INPUT_FILE=""
NON_INTERACTIVE=false
SKIP_TESTS=false
UPDATE_REQUESTED=false
RECONFIGURE_REQUESTED=false
UNINSTALL_REQUESTED=false
ACTION="install"

CONFIG_DIR=""
TOKEN_PATH=""
TEMP_DIR=""
STAGED_BINARY=""
BUILD_DIR=""
CARGO_BIN=""
RUSTC_BIN=""
TOKEN_TEMP=""
CONFIG_TEMP=""
UNIT_TEMP=""
OVERRIDE_TEMP=""
SOURCE_STAGE_ROOT=""
STAGED_SOURCE=""
GIT_SAFE_CONFIG=""
SOURCE_BACKUP=""
SOURCE_HAD_EXISTING=false
ROLLBACK_DIR=""
PRESERVE_ROLLBACK=false
TRANSACTION_ACTIVE=false
SERVICE_WAS_ACTIVE=false
SERVICE_WAS_ENABLED=false
SERVICE_USER_HAD_EXISTING=false
SERVICE_GROUP_HAD_EXISTING=false
BUILD_ACCOUNT_VALIDATED=false
SERVICE_ACCOUNT_VALIDATED=false
CONFIG_DIR_HAD_EXISTING=false
CONFIG_DIR_METADATA=""
STATE_DIR_HAD_EXISTING=false
STATE_DIR_METADATA=""
UNIT_OVERRIDE_DIR_HAD_EXISTING=false
UNIT_OVERRIDE_DIR_METADATA=""
INCOMPLETE_INSTALL=false

INPUT_TOKEN_FILE=""
INPUT_TOKEN_VALUE=""
INPUT_ZONE=""
INPUT_RECORD=""
INPUT_IPV4=""
INPUT_IPV6=""
INPUT_INTERVAL=""
INPUT_CREATE=""
INPUT_PROXIED=""

log() {
    printf '[cirrusync] %s\n' "$*"
}

warn() {
    printf '[cirrusync] WARNING: %s\n' "$*" >&2
}

report_rollback_failure() {
    local step="${1:-unknown}"
    local status="${2:-}"

    case "${step}" in
        "" | *[!0123456789abcdefghijklmnopqrstuvwxyz-]*) step="unknown" ;;
    esac
    if [[ "${status}" =~ ^[1-9][0-9]*$ ]]; then
        warn "Rollback step failed: ${step} (exit ${status})" || true
    else
        warn "Rollback step failed: ${step}" || true
    fi
    return 0
}

die() {
    printf '[cirrusync] ERROR: %s\n' "$*" >&2
    exit 1
}

read_from_tty() {
    [[ -c /dev/tty && -r /dev/tty ]] ||
        die "interactive input requires a controlling terminal; use --non-interactive"
    IFS= read -r "$@" </dev/tty ||
        die "interactive input ended unexpectedly"
}

on_error() {
    local exit_code="$1"
    local line="$2"
    local command="$3"

    # Commands that handle secrets never put secret values in argv. BASH_COMMAND
    # therefore gives useful context without expanding the token itself.
    printf '[cirrusync] ERROR: command failed at line %s (exit %s): %s\n' \
        "$line" "$exit_code" "$command" >&2
    exit "$exit_code"
}

cleanup() {
    trap - ERR
    set +e
    if [[ -n "${TOKEN_TEMP}" && -e "${TOKEN_TEMP}" ]]; then
        rm -f -- "${TOKEN_TEMP}"
    fi
    if [[ -n "${CONFIG_TEMP}" && -e "${CONFIG_TEMP}" ]]; then
        rm -f -- "${CONFIG_TEMP}"
    fi
    if [[ -n "${UNIT_TEMP}" && -e "${UNIT_TEMP}" ]]; then
        rm -f -- "${UNIT_TEMP}"
    fi
    if [[ -n "${OVERRIDE_TEMP}" && -e "${OVERRIDE_TEMP}" ]]; then
        rm -f -- "${OVERRIDE_TEMP}"
    fi
    if [[ "${TRANSACTION_ACTIVE}" == true ]]; then
        rollback_transaction
    fi
    if [[ "${BUILD_ACCOUNT_VALIDATED}" == true ]]; then
        terminate_account_processes "${BUILD_USER}" ||
            warn "Could not terminate every process owned by ${BUILD_USER}"
    fi
    if [[ -n "${STAGED_BINARY}" && -e "${STAGED_BINARY}" ]]; then
        rm -f -- "${STAGED_BINARY}"
    fi
    if [[ -n "${BUILD_DIR}" && -d "${BUILD_DIR}" ]]; then
        rm -rf -- "${BUILD_DIR}"
    fi
    if [[ -n "${SOURCE_STAGE_ROOT}" && -d "${SOURCE_STAGE_ROOT}" ]]; then
        rm -rf -- "${SOURCE_STAGE_ROOT}"
    fi
    if [[ -n "${GIT_SAFE_CONFIG}" && -e "${GIT_SAFE_CONFIG}" ]]; then
        rm -f -- "${GIT_SAFE_CONFIG}"
    fi
    if [[ -n "${TEMP_DIR}" && -d "${TEMP_DIR}" ]]; then
        rm -rf -- "${TEMP_DIR}"
    fi
    if [[ "${PRESERVE_ROLLBACK}" == false &&
        -n "${ROLLBACK_DIR}" && -d "${ROLLBACK_DIR}" ]]; then
        rm -rf -- "${ROLLBACK_DIR}"
    fi
}

snapshot_path() {
    local path="$1"
    local key="$2"

    if [[ -e "${path}" || -L "${path}" ]]; then
        cp --archive --no-dereference -- "${path}" "${ROLLBACK_DIR}/${key}"
        : >"${ROLLBACK_DIR}/${key}.present"
    fi
}

restore_snapshot() {
    local path="$1"
    local key="$2"
    local temporary=""

    if [[ ! -e "${ROLLBACK_DIR}/${key}.present" ]]; then
        rm -f -- "${path}"
        return 0
    fi

    mkdir -p -- "$(dirname -- "${path}")" || return 1
    temporary="$(mktemp "${path}.restore.XXXXXX")" || return 1
    rm -f -- "${temporary}" || return 1
    if ! cp --archive --no-dereference -- \
        "${ROLLBACK_DIR}/${key}" "${temporary}"; then
        rm -f -- "${temporary}"
        return 1
    fi
    if ! mv --force --no-target-directory -- "${temporary}" "${path}"; then
        rm -f -- "${temporary}"
        return 1
    fi
}

begin_transaction() {
    local activity=""
    local enablement=""
    local service_processes=""
    local service_uid=""

    [[ "${TRANSACTION_ACTIVE}" == false ]] ||
        die "internal error: an installation transaction is already active"

    # Reject known-unsafe directory metadata before an active service is
    # stopped. The hardening step repeats this after the transaction begins.
    validate_configuration_directory_acl

    PRESERVE_ROLLBACK=false
    ROLLBACK_DIR="$(mktemp -d /var/tmp/cirrusync-rollback.XXXXXX)"
    chmod 0700 "${ROLLBACK_DIR}"
    snapshot_path "${BINARY_PATH}" binary
    snapshot_path "${UNIT_PATH}" unit
    snapshot_path "${UNIT_OVERRIDE_PATH}" override
    snapshot_path "${CONFIG_PATH}" config
    snapshot_path "${TOKEN_PATH}" token

    SERVICE_USER_HAD_EXISTING=false
    SERVICE_GROUP_HAD_EXISTING=false
    CONFIG_DIR_HAD_EXISTING=false
    CONFIG_DIR_METADATA=""
    STATE_DIR_HAD_EXISTING=false
    STATE_DIR_METADATA=""
    UNIT_OVERRIDE_DIR_HAD_EXISTING=false
    UNIT_OVERRIDE_DIR_METADATA=""
    getent passwd "${SERVICE_USER}" >/dev/null 2>&1 &&
        SERVICE_USER_HAD_EXISTING=true
    getent group "${SERVICE_GROUP}" >/dev/null 2>&1 &&
        SERVICE_GROUP_HAD_EXISTING=true
    if [[ -d "${CONFIG_DIR}" ]]; then
        CONFIG_DIR_HAD_EXISTING=true
        CONFIG_DIR_METADATA="$(stat -c '%u:%g:%a' "${CONFIG_DIR}")"
    fi
    if [[ -d "${STATE_DIR}" ]]; then
        STATE_DIR_HAD_EXISTING=true
        STATE_DIR_METADATA="$(stat -c '%u:%g:%a' "${STATE_DIR}")"
    fi
    if [[ -d "${UNIT_OVERRIDE_DIR}" ]]; then
        UNIT_OVERRIDE_DIR_HAD_EXISTING=true
        UNIT_OVERRIDE_DIR_METADATA="$(stat -c '%u:%g:%a' "${UNIT_OVERRIDE_DIR}")"
    fi

    SERVICE_WAS_ACTIVE=false
    SERVICE_WAS_ENABLED=false
    if systemd_is_running; then
        activity="$(read_service_activity)" ||
            die "could not determine the prior activity state of ${SERVICE_NAME}"
        enablement="$(read_service_enablement)" ||
            die "could not determine the prior enablement state of ${SERVICE_NAME}"
        [[ "${activity}" == active ]] && SERVICE_WAS_ACTIVE=true
        [[ "${enablement}" == enabled ]] && SERVICE_WAS_ENABLED=true
    fi
    TRANSACTION_ACTIVE=true

    if [[ "${SERVICE_WAS_ACTIVE}" == true ]]; then
        log "Stopping ${SERVICE_NAME} while transactional files are replaced"
        systemctl stop "${SERVICE_NAME}" >/dev/null 2>&1 || true
        wait_for_service_quiescence ||
            die "${SERVICE_NAME} did not stop cleanly"
    fi

    if getent passwd "${SERVICE_USER}" >/dev/null 2>&1; then
        service_uid="$(id -u "${SERVICE_USER}")" ||
            die "could not resolve the numeric identity of ${SERVICE_USER}"
        service_processes="$(list_account_processes "${service_uid}")" ||
            die "could not inspect processes owned by ${SERVICE_USER}"
        [[ -z "${service_processes}" ]] ||
            die "refusing to replace runtime files while a process owned by ${SERVICE_USER} is running"
    fi
}

rollback_transaction() {
    local rollback_failed=false
    local directory_uid=""
    local directory_gid=""
    local directory_mode=""
    local expected_restarts=""
    local failed_source=""
    local retain_new_service_identity=false
    local rollback_status=""

    [[ "${TRANSACTION_ACTIVE}" == true ]] || return 0
    warn "Installation did not complete; restoring the last-known-good files and service state"
    TRANSACTION_ACTIVE=false

    if systemd_is_running && ! quiesce_service; then
        PRESERVE_ROLLBACK=true
        warn "Rollback could not safely quiesce ${SERVICE_NAME}; live files were not modified and recovery data was preserved at ${ROLLBACK_DIR}"
        return 1
    fi
    if [[ "${SERVICE_ACCOUNT_VALIDATED}" == true ]]; then
        if ! terminate_account_processes "${SERVICE_USER}"; then
            PRESERVE_ROLLBACK=true
            warn "Rollback could not verify that ${SERVICE_USER} is idle; live files were not modified and recovery data was preserved at ${ROLLBACK_DIR}"
            return 1
        fi
    fi

    restore_snapshot "${TOKEN_PATH}" token || {
        report_rollback_failure restore-token "$?"
        rollback_failed=true
    }
    restore_snapshot "${CONFIG_PATH}" config || {
        report_rollback_failure restore-config "$?"
        rollback_failed=true
    }
    restore_snapshot "${BINARY_PATH}" binary || {
        report_rollback_failure restore-binary "$?"
        rollback_failed=true
    }
    restore_snapshot "${UNIT_PATH}" unit || {
        report_rollback_failure restore-unit "$?"
        rollback_failed=true
    }
    restore_snapshot "${UNIT_OVERRIDE_PATH}" override || {
        report_rollback_failure restore-unit-override "$?"
        rollback_failed=true
    }
    if [[ "${UNIT_OVERRIDE_DIR_HAD_EXISTING}" == true ]]; then
        mkdir -p -- "${UNIT_OVERRIDE_DIR}" || {
            report_rollback_failure restore-unit-override-dir "$?"
            rollback_failed=true
        }
        IFS=: read -r directory_uid directory_gid directory_mode \
            <<<"${UNIT_OVERRIDE_DIR_METADATA}"
        chown "${directory_uid}:${directory_gid}" "${UNIT_OVERRIDE_DIR}" || {
            report_rollback_failure restore-unit-override-dir-owner "$?"
            rollback_failed=true
        }
        chmod "${directory_mode}" "${UNIT_OVERRIDE_DIR}" || {
            report_rollback_failure restore-unit-override-dir-mode "$?"
            rollback_failed=true
        }
    else
        rmdir --ignore-fail-on-non-empty "${UNIT_OVERRIDE_DIR}" \
            >/dev/null 2>&1 || true
    fi

    if [[ -n "${SOURCE_BACKUP}" && -e "${SOURCE_BACKUP}" ]]; then
        failed_source="${SOURCE_DIR}.failed.${BASHPID}"
        if [[ -e "${failed_source}" || -L "${failed_source}" ]]; then
            report_rollback_failure preserve-failed-source
            rollback_failed=true
        elif [[ -e "${SOURCE_DIR}" || -L "${SOURCE_DIR}" ]] &&
            ! mv -- "${SOURCE_DIR}" "${failed_source}"; then
            report_rollback_failure preserve-failed-source
            rollback_failed=true
        elif mv -- "${SOURCE_BACKUP}" "${SOURCE_DIR}"; then
            SOURCE_BACKUP=""
            if [[ -e "${failed_source}" || -L "${failed_source}" ]]; then
                rm -rf -- "${failed_source}" || {
                    report_rollback_failure remove-failed-source "$?"
                    rollback_failed=true
                }
            fi
        else
            report_rollback_failure restore-source
            rollback_failed=true
            if [[ -e "${failed_source}" || -L "${failed_source}" ]]; then
                mv -- "${failed_source}" "${SOURCE_DIR}" || {
                    report_rollback_failure recover-failed-source "$?"
                    rollback_failed=true
                }
            fi
        fi
    elif [[ "${SOURCE_HAD_EXISTING}" == false &&
        -n "${STAGED_SOURCE}" && "${STAGED_SOURCE}" == "${SOURCE_DIR}" ]]; then
        rm -rf -- "${SOURCE_DIR}" || {
            report_rollback_failure remove-new-source "$?"
            rollback_failed=true
        }
    fi

    if [[ "${CONFIG_DIR_HAD_EXISTING}" == true ]]; then
        IFS=: read -r directory_uid directory_gid directory_mode \
            <<<"${CONFIG_DIR_METADATA}"
        chown "${directory_uid}:${directory_gid}" "${CONFIG_DIR}" || {
            report_rollback_failure restore-config-dir-owner "$?"
            rollback_failed=true
        }
        chmod "${directory_mode}" "${CONFIG_DIR}" || {
            report_rollback_failure restore-config-dir-mode "$?"
            rollback_failed=true
        }
    elif [[ -d "${CONFIG_DIR}" ]] &&
        ! rmdir --ignore-fail-on-non-empty "${CONFIG_DIR}" 2>/dev/null; then
        report_rollback_failure remove-new-config-dir
        rollback_failed=true
    fi
    if [[ "${STATE_DIR_HAD_EXISTING}" == true ]]; then
        if [[ "${SERVICE_USER_HAD_EXISTING}" == false ]]; then
            retain_new_service_identity=true
        fi
        IFS=: read -r directory_uid directory_gid directory_mode \
            <<<"${STATE_DIR_METADATA}"
        chown "${directory_uid}:${directory_gid}" "${STATE_DIR}" || {
            report_rollback_failure restore-state-dir-owner "$?"
            rollback_failed=true
        }
        chmod "${directory_mode}" "${STATE_DIR}" || {
            report_rollback_failure restore-state-dir-mode "$?"
            rollback_failed=true
        }
    elif [[ -d "${STATE_DIR}" ]]; then
        if remove_new_state_directory; then
            :
        else
            rollback_status="$?"
            report_rollback_failure remove-new-state-dir "${rollback_status}"
            rollback_failed=true
            retain_new_service_identity=true
        fi
    fi

    if [[ "${SERVICE_USER_HAD_EXISTING}" == false &&
        "${retain_new_service_identity}" == false ]] &&
        getent passwd "${SERVICE_USER}" >/dev/null 2>&1; then
        if (
            validate_local_system_group "${SERVICE_GROUP}"
            validate_local_system_user \
                "${SERVICE_USER}" "${SERVICE_GROUP}" "${STATE_DIR}"
        ); then
            userdel "${SERVICE_USER}" || {
                report_rollback_failure delete-new-service-user "$?"
                rollback_failed=true
            }
        else
            report_rollback_failure validate-new-service-identity
            rollback_failed=true
        fi
    fi
    if [[ "${SERVICE_GROUP_HAD_EXISTING}" == false &&
        "${retain_new_service_identity}" == false ]] &&
        getent group "${SERVICE_GROUP}" >/dev/null 2>&1; then
        if (validate_local_system_group "${SERVICE_GROUP}"); then
            groupdel "${SERVICE_GROUP}" || {
                report_rollback_failure delete-new-service-group "$?"
                rollback_failed=true
            }
        else
            report_rollback_failure validate-new-service-group
            rollback_failed=true
        fi
    fi

    if systemd_is_running; then
        systemctl daemon-reload >/dev/null 2>&1 || {
            report_rollback_failure systemd-daemon-reload "$?"
            rollback_failed=true
        }
        restore_service_enablement "${SERVICE_WAS_ENABLED}" || {
            report_rollback_failure restore-service-enablement "$?"
            rollback_failed=true
        }
        if [[ "${SERVICE_WAS_ACTIVE}" == true &&
            "${rollback_failed}" == false ]]; then
            systemctl reset-failed "${SERVICE_NAME}" >/dev/null 2>&1 || true
            if expected_restarts="$(systemctl show \
                --property=NRestarts --value \
                "${SERVICE_NAME}" 2>/dev/null)"; then
                if [[ "${expected_restarts}" =~ ^[0-9]+$ ]]; then
                    if ! systemctl restart "${SERVICE_NAME}" >/dev/null 2>&1; then
                        report_rollback_failure restart-prior-service
                        rollback_failed=true
                        quiesce_failed_service || {
                            report_rollback_failure quiesce-failed-service "$?"
                            rollback_failed=true
                        }
                    elif ! service_remains_healthy 5 "${expected_restarts}"; then
                        report_rollback_failure verify-restarted-service
                        rollback_failed=true
                        quiesce_failed_service || {
                            report_rollback_failure quiesce-failed-service "$?"
                            rollback_failed=true
                        }
                    fi
                else
                    report_rollback_failure inspect-prior-service-restarts
                    rollback_failed=true
                fi
            else
                report_rollback_failure inspect-prior-service-restarts "$?"
                rollback_failed=true
            fi
        elif [[ "${SERVICE_WAS_ACTIVE}" == true ]]; then
            warn "Rollback left ${SERVICE_NAME} stopped because managed state could not be restored safely"
        fi
    fi

    if [[ "${rollback_failed}" == true ]]; then
        PRESERVE_ROLLBACK=true
        warn "Rollback encountered an error; preserved recovery data at ${ROLLBACK_DIR}"
        return 1
    fi
    rm -rf -- "${ROLLBACK_DIR}" || {
        PRESERVE_ROLLBACK=true
        warn "Rollback succeeded, but recovery workspace cleanup failed: ${ROLLBACK_DIR}"
        return 1
    }
    ROLLBACK_DIR=""
    log "Rollback completed"
}

commit_transaction() {
    [[ "${TRANSACTION_ACTIVE}" == true ]] ||
        die "internal error: no installation transaction is active"

    # Deployment is already healthy at this point. Cleanup failures must not
    # trigger a rollback from a backup that may have been partly removed.
    TRANSACTION_ACTIVE=false
    if [[ -n "${SOURCE_BACKUP}" && -e "${SOURCE_BACKUP}" ]]; then
        if rm -rf -- "${SOURCE_BACKUP}"; then
            SOURCE_BACKUP=""
        else
            warn "Could not remove stale source backup ${SOURCE_BACKUP}; remove it manually"
        fi
    fi
    if rm -rf -- "${ROLLBACK_DIR}"; then
        ROLLBACK_DIR=""
    else
        PRESERVE_ROLLBACK=true
        warn "Could not remove rollback workspace ${ROLLBACK_DIR}; remove it manually"
    fi
}

if [[ "${BOOTSTRAP_IS_SOURCED}" == false ]]; then
    trap 'on_error "$?" "$LINENO" "$BASH_COMMAND"' ERR
    trap cleanup EXIT
fi

usage() {
    cat <<'EOF'
Cirrusync bootstrap installer

Usage:
  sudo ./bootstrap.sh [options]
  curl -fsSL https://raw.githubusercontent.com/wiresock/cirrusync/main/bootstrap.sh |
    sudo bash -s -- [options]

Options:
  --repo URL          Git repository to clone
                      (default: https://github.com/wiresock/cirrusync.git).
  --branch BRANCH     Git branch to install (default: main).
  --config PATH       Installed configuration path
                      (default: /etc/cirrusync/config.toml).
  --token-file PATH   Read the Cloudflare token from this local file.
                      The source file is not deleted.
  --non-interactive   Do not prompt. Required values must be supplied through
                      options or environment variables.
  --skip-tests        Build without running cargo test --locked.
  --update            Update the managed source and binary without replacing
                      an existing configuration or token.
  --reconfigure       Replace configuration non-interactively or select
                      reconfiguration directly; explicit values are required.
  --uninstall         Remove the service, unit, and binary. Configuration and
                      secrets are retained unless deletion is confirmed.
  -h, --help          Show this help.

Non-interactive environment:
  CIRRUSYNC_TOKEN_FILE  Secure token source file (preferred).
  CIRRUSYNC_TOKEN       Token value (less safe; may be exposed to privileged
                        process inspection or inherited environments).
  CIRRUSYNC_ZONE        Cloudflare zone, for example example.com.
  CIRRUSYNC_RECORD      Record name, for example home.example.com.
  CIRRUSYNC_ENABLE_IPV4 true or false (default: true).
  CIRRUSYNC_ENABLE_IPV6 true or false (default: false).
  CIRRUSYNC_INTERVAL    Update interval in seconds (default: 300).
  CIRRUSYNC_CREATE      Create a missing record (default: false).
  CIRRUSYNC_PROXIED     Enable Cloudflare proxying (default: false).

The equivalent CFDDNS_* names are accepted as compatibility aliases. Existing
configuration and token files are preserved in non-interactive update mode.
After an incomplete installation, explicitly supplied values replace the
corresponding incomplete files.
EOF
}

parse_arguments() {
    while (($# > 0)); do
        case "$1" in
            --repo)
                (($# >= 2)) || die "--repo requires a value"
                REPO_URL="$2"
                shift 2
                ;;
            --branch)
                (($# >= 2)) || die "--branch requires a value"
                BRANCH="$2"
                shift 2
                ;;
            --config)
                (($# >= 2)) || die "--config requires a value"
                CONFIG_PATH="$2"
                shift 2
                ;;
            --token-file)
                (($# >= 2)) || die "--token-file requires a value"
                TOKEN_INPUT_FILE="$2"
                shift 2
                ;;
            --non-interactive)
                NON_INTERACTIVE=true
                shift
                ;;
            --skip-tests)
                SKIP_TESTS=true
                shift
                ;;
            --update)
                UPDATE_REQUESTED=true
                shift
                ;;
            --reconfigure)
                RECONFIGURE_REQUESTED=true
                shift
                ;;
            --uninstall)
                UNINSTALL_REQUESTED=true
                shift
                ;;
            -h | --help)
                usage
                exit 0
                ;;
            --)
                shift
                (($# == 0)) || die "unexpected positional argument: $1"
                ;;
            -*)
                die "unknown option: $1"
                ;;
            *)
                die "unexpected positional argument: $1"
                ;;
        esac
    done

    if [[ "${UPDATE_REQUESTED}" == true && "${RECONFIGURE_REQUESTED}" == true ]] ||
        [[ "${UPDATE_REQUESTED}" == true && "${UNINSTALL_REQUESTED}" == true ]] ||
        [[ "${RECONFIGURE_REQUESTED}" == true && "${UNINSTALL_REQUESTED}" == true ]]; then
        die "--update, --reconfigure, and --uninstall are mutually exclusive"
    fi
}

ensure_root() {
    if ((EUID == 0)); then
        return
    fi

    command -v sudo >/dev/null 2>&1 ||
        die "run this installer as root, or install sudo"

    if [[ -f "${BASH_SOURCE[0]}" && -r "${BASH_SOURCE[0]}" ]]; then
        log "Re-executing the installer through sudo"
        exec sudo \
            --preserve-env=CIRRUSYNC_TOKEN_FILE,CIRRUSYNC_TOKEN,CIRRUSYNC_ZONE,CIRRUSYNC_RECORD,CIRRUSYNC_ENABLE_IPV4,CIRRUSYNC_ENABLE_IPV6,CIRRUSYNC_INTERVAL,CIRRUSYNC_CREATE,CIRRUSYNC_PROXIED,CFDDNS_TOKEN_FILE,CFDDNS_TOKEN,CFDDNS_ZONE,CFDDNS_RECORD,CFDDNS_ENABLE_IPV4,CFDDNS_ENABLE_IPV6,CFDDNS_INTERVAL,CFDDNS_CREATE,CFDDNS_PROXIED \
            bash "${BASH_SOURCE[0]}" "$@"
    fi

    die "piped installs must put sudo before bash: curl ... | sudo bash"
}

capture_install_environment() {
    # Copy installer inputs into non-exported shell variables, then remove them
    # from the environment before invoking apt, Git, Cargo, tests, or the daemon.
    INPUT_TOKEN_FILE="${CIRRUSYNC_TOKEN_FILE:-${CFDDNS_TOKEN_FILE:-}}"
    INPUT_TOKEN_VALUE="${CIRRUSYNC_TOKEN:-${CFDDNS_TOKEN:-}}"
    INPUT_ZONE="${CIRRUSYNC_ZONE:-${CFDDNS_ZONE:-}}"
    INPUT_RECORD="${CIRRUSYNC_RECORD:-${CFDDNS_RECORD:-}}"
    INPUT_IPV4="${CIRRUSYNC_ENABLE_IPV4:-${CFDDNS_ENABLE_IPV4:-}}"
    INPUT_IPV6="${CIRRUSYNC_ENABLE_IPV6:-${CFDDNS_ENABLE_IPV6:-}}"
    INPUT_INTERVAL="${CIRRUSYNC_INTERVAL:-${CFDDNS_INTERVAL:-}}"
    INPUT_CREATE="${CIRRUSYNC_CREATE:-${CFDDNS_CREATE:-}}"
    INPUT_PROXIED="${CIRRUSYNC_PROXIED:-${CFDDNS_PROXIED:-}}"

    unset \
        CIRRUSYNC_TOKEN_FILE CIRRUSYNC_TOKEN CIRRUSYNC_ZONE CIRRUSYNC_RECORD \
        CIRRUSYNC_ENABLE_IPV4 CIRRUSYNC_ENABLE_IPV6 CIRRUSYNC_INTERVAL \
        CIRRUSYNC_CREATE CIRRUSYNC_PROXIED \
        CFDDNS_TOKEN_FILE CFDDNS_TOKEN CFDDNS_ZONE CFDDNS_RECORD \
        CFDDNS_ENABLE_IPV4 CFDDNS_ENABLE_IPV6 CFDDNS_INTERVAL \
        CFDDNS_CREATE CFDDNS_PROXIED
}

acquire_installer_lock() {
    command -v flock >/dev/null 2>&1 ||
        die "flock is required (install the util-linux package)"
    [[ ! -L "${INSTALL_LOCK_PATH}" ]] ||
        die "refusing to use a symbolic link as the installer lock"
    if [[ -e "${INSTALL_LOCK_PATH}" ]]; then
        [[ -f "${INSTALL_LOCK_PATH}" ]] ||
            die "the installer lock path is not a regular file"
        [[ "$(stat -c '%u' "${INSTALL_LOCK_PATH}")" == 0 ]] ||
            die "the installer lock is not owned by root"
    fi

    (
        umask 0077
        : >"${INSTALL_LOCK_PATH}"
    )
    chmod 0600 "${INSTALL_LOCK_PATH}"
    # The descriptor deliberately remains open for the lifetime of the script.
    exec 9>"${INSTALL_LOCK_PATH}"
    flock --nonblock 9 ||
        die "another Cirrusync installer or uninstaller is already running"
}

assert_directory_chain_is_safe() {
    local path="$1"
    local current=""
    local component=""
    local -a components=()
    local IFS='/'

    read -r -a components <<<"${path#/}"
    for component in "${components[@]}"; do
        [[ -n "${component}" ]] || continue
        current="${current}/${component}"
        [[ ! -L "${current}" ]] ||
            die "refusing path with symbolic-link component: ${current}"
        if [[ -e "${current}" && ! -d "${current}" ]]; then
            die "path component is not a directory: ${current}"
        fi
    done
}

validate_trusted_directory_chain() {
    local path="$1"
    local current=""
    local component=""
    local -a components=()
    local IFS='/'

    assert_directory_chain_is_safe "${path}"
    read -r -a components <<<"${path#/}"
    for component in "${components[@]}"; do
        [[ -n "${component}" ]] || continue
        current="${current}/${component}"
        validate_root_owned_directory "${current}"
    done
}

validate_root_owned_directory() {
    local path="$1"
    local mode=""

    [[ ! -L "${path}" ]] ||
        die "refusing symbolic-link directory: ${path}"
    [[ ! -e "${path}" || -d "${path}" ]] ||
        die "expected a directory at ${path}"
    if [[ -d "${path}" ]]; then
        [[ "$(stat -c '%u' "${path}")" == 0 ]] ||
            die "${path} must be owned by root"
        mode="$(stat -c '%a' "${path}")"
        (( (8#${mode} & 0022) == 0 )) ||
            die "${path} must not be writable by group or other users"
    fi
}

validate_root_owned_regular_file() {
    local path="$1"
    local mode=""

    [[ -f "${path}" && ! -L "${path}" ]] ||
        die "expected a regular, non-symlink file at ${path}"
    [[ "$(stat -c '%u' "${path}")" == 0 &&
        "$(stat -c '%h' "${path}")" == 1 ]] ||
        die "${path} must be a single-link root-owned file"
    mode="$(stat -c '%a' "${path}")"
    (( (8#${mode} & 06022) == 0 )) ||
        die "${path} must not have set-ID bits or be writable by group or other users"
}

terminate_account_processes() {
    local user_name="$1"
    local user_uid=""
    local attempt=""
    local process_list=""

    [[ "${user_name}" == "${BUILD_USER}" ||
        "${user_name}" == "${SERVICE_USER}" ]] || return 1
    getent passwd "${user_name}" >/dev/null 2>&1 || return 0
    user_uid="$(id -u "${user_name}")" || return 1
    [[ "${user_uid}" =~ ^[0-9]+$ && "${user_uid}" -ne 0 ]] || return 1
    command -v pkill >/dev/null 2>&1 || return 1

    process_list="$(list_account_processes "${user_uid}")" || return 1
    if [[ -z "${process_list}" ]]; then
        return 0
    fi
    warn "Terminating leftover processes owned by the dedicated ${user_name} account"
    pkill --signal TERM --euid "${user_uid}" -- '.*' 2>/dev/null || true
    for ((attempt = 0; attempt < 5; attempt++)); do
        process_list="$(list_account_processes "${user_uid}")" || return 1
        [[ -z "${process_list}" ]] && return 0
        sleep 1
    done
    pkill --signal KILL --euid "${user_uid}" -- '.*' 2>/dev/null || true
    for ((attempt = 0; attempt < 5; attempt++)); do
        process_list="$(list_account_processes "${user_uid}")" || return 1
        [[ -z "${process_list}" ]] && return 0
        sleep 1
    done
    return 1
}

list_account_processes() {
    local user_uid="$1"
    local process_list=""
    local status=""

    [[ "${user_uid}" =~ ^[0-9]+$ ]] || return 1
    command -v pgrep >/dev/null 2>&1 || return 1
    if process_list="$(pgrep --euid "${user_uid}" -- '.*' 2>/dev/null)"; then
        :
    else
        status="$?"
        ((status == 1)) || return 1
        process_list=""
    fi
    printf '%s' "${process_list}"
}

run_without_privilege_gain() {
    local user_name="$1"
    shift

    [[ "${user_name}" == "${BUILD_USER}" ||
        "${user_name}" == "${SERVICE_USER}" ]] || return 1
    command -v setpriv >/dev/null 2>&1 ||
        die "setpriv is required to isolate unprivileged commands"
    runuser --user "${user_name}" -- setpriv --no-new-privs "$@"
}

systemd_is_running() {
    command -v systemctl >/dev/null 2>&1 &&
        [[ -d /run/systemd/system ]]
}

service_is_quiescent() {
    local activity=""

    activity="$(read_service_activity)" || return 1
    [[ "${activity}" == inactive ]]
}

read_service_activity() {
    local load_state=""
    local active_state=""

    load_state="$(systemctl show --property=LoadState --value \
        "${SERVICE_NAME}" 2>/dev/null)" || return 1
    if [[ "${load_state}" == not-found ]]; then
        printf '%s' inactive
        return 0
    fi
    [[ -n "${load_state}" ]] || return 1
    active_state="$(systemctl is-active "${SERVICE_NAME}" 2>/dev/null)" || true
    case "${active_state}" in
        active | activating | reloading | deactivating) printf '%s' active ;;
        inactive | failed) printf '%s' inactive ;;
        *) return 1 ;;
    esac
}

read_service_enablement() {
    local enable_state=""

    enable_state="$(systemctl is-enabled "${SERVICE_NAME}" 2>/dev/null)" || true
    case "${enable_state}" in
        enabled | enabled-runtime | linked | linked-runtime | alias)
            printf '%s' enabled
            ;;
        disabled | static | indirect | generated | transient | masked | masked-runtime | not-found)
            printf '%s' disabled
            ;;
        *) return 1 ;;
    esac
}

wait_for_service_quiescence() {
    local attempt=""

    for ((attempt = 0; attempt < 10; attempt++)); do
        service_is_quiescent && return 0
        sleep 1
    done
    service_is_quiescent
}

quiesce_service() {
    local cleanup_failed=false
    local service_processes=""
    local service_uid=""

    systemctl stop "${SERVICE_NAME}" >/dev/null 2>&1 || true
    wait_for_service_quiescence || true

    if [[ "${SERVICE_ACCOUNT_VALIDATED}" == true ]]; then
        terminate_account_processes "${SERVICE_USER}" || cleanup_failed=true
    elif getent passwd "${SERVICE_USER}" >/dev/null 2>&1; then
        service_uid="$(id -u "${SERVICE_USER}")" || cleanup_failed=true
        if [[ -n "${service_uid}" ]]; then
            service_processes="$(list_account_processes "${service_uid}")" ||
                cleanup_failed=true
            [[ -z "${service_processes}" ]] || cleanup_failed=true
        fi
    fi
    wait_for_service_quiescence || return 1
    [[ "${cleanup_failed}" == false ]]
}

service_enablement_matches() {
    local should_be_enabled="$1"
    local enable_state=""

    enable_state="$(read_service_enablement)" || return 1
    if [[ "${should_be_enabled}" == true ]]; then
        [[ "${enable_state}" == enabled ]]
    else
        [[ "${enable_state}" == disabled ]]
    fi
}

restore_service_enablement() {
    local should_be_enabled="$1"

    if [[ "${should_be_enabled}" == true ]]; then
        systemctl enable "${SERVICE_NAME}" >/dev/null 2>&1 || true
    else
        systemctl disable "${SERVICE_NAME}" >/dev/null 2>&1 || true
    fi
    service_enablement_matches "${should_be_enabled}"
}

quiesce_failed_service() {
    quiesce_service
}

remove_new_state_directory() (
    local service_uid=""
    local entry=""
    local name=""
    local mode=""
    local -a entries=()

    [[ "${STATE_DIR}" == "${BOOTSTRAP_ROOT}/var/lib/cirrusync" &&
        -d "${STATE_DIR}" && ! -L "${STATE_DIR}" ]] || return 10
    service_uid="$(id -u "${SERVICE_USER}")" || return 11
    [[ "$(stat -c '%u' "${STATE_DIR}")" == "${service_uid}" ]] || return 12

    shopt -s nullglob dotglob
    entries=("${STATE_DIR}"/*)
    for entry in "${entries[@]}"; do
        name="${entry##*/}"
        [[ "${name}" =~ ^record-[0-9a-f]{16}\.lock$ ]] || return 13
        [[ -f "${entry}" && ! -L "${entry}" &&
            "$(stat -c '%u' "${entry}")" == "${service_uid}" &&
            "$(stat -c '%h' "${entry}")" == 1 ]] || return 14
        mode="$(stat -c '%a' "${entry}")" || return 15
        [[ "${mode}" =~ ^[0-7]{3,4}$ ]] || return 15
        (( (8#${mode} & 0077) == 0 )) || return 16
        rm -f -- "${entry}" || return 17
    done
    rmdir -- "${STATE_DIR}" || return 18
)

refuse_mount_point() {
    local path="$1"
    local mount_targets=""
    local resolved_path=""
    local target=""

    [[ ! -e "${path}" && ! -L "${path}" ]] && return 0
    command -v findmnt >/dev/null 2>&1 ||
        die "findmnt is required before recursively removing managed directories"
    resolved_path="$(readlink -f -- "${path}")" ||
        die "could not resolve managed directory before removal: ${path}"
    mount_targets="$(findmnt --kernel --raw --noheadings --output TARGET)" ||
        die "could not inspect mounted filesystems before removing ${path}"
    while IFS= read -r target; do
        if [[ "${target}" == "${resolved_path}" ||
            "${target}" == "${resolved_path}/"* ]]; then
            die "refusing to recursively remove a path containing a mount: ${target}"
        fi
    done <<<"${mount_targets}"
}

validate_managed_paths() {
    local path=""
    local mode=""

    for path in \
        "${BINARY_PATH%/*}" \
        "${SOURCE_DIR%/*}" \
        "${STATE_DIR%/*}" \
        "${BUILD_STATE_DIR%/*}" \
        "${UNIT_PATH%/*}"; do
        validate_trusted_directory_chain "${path}"
    done

    for path in "${BINARY_PATH}" "${UNIT_PATH}" "${UNIT_OVERRIDE_PATH}"; do
        [[ ! -L "${path}" ]] ||
            die "refusing symbolic-link managed file: ${path}"
        [[ ! -e "${path}" || -f "${path}" ]] ||
            die "managed file path has an unexpected type: ${path}"
        if [[ -f "${path}" ]]; then
            [[ "$(stat -c '%u' "${path}")" == 0 &&
                "$(stat -c '%h' "${path}")" == 1 ]] ||
                die "managed file must be a single-link root-owned file: ${path}"
            mode="$(stat -c '%a' "${path}")"
            (( (8#${mode} & 06022) == 0 )) ||
                die "managed file must not have set-ID bits or be writable by group or other users: ${path}"
        fi
    done

    for path in \
        "${SOURCE_DIR}" \
        "${BUILD_STATE_DIR}" \
        "${BUILD_STATE_DIR}/cargo" \
        "${BUILD_STATE_DIR}/rustup" \
        "${UNIT_OVERRIDE_DIR}"; do
        validate_root_owned_directory "${path}"
    done
    [[ ! -L "${STATE_DIR}" ]] ||
        die "refusing symbolic-link state directory: ${STATE_DIR}"
    [[ ! -e "${STATE_DIR}" || -d "${STATE_DIR}" ]] ||
        die "expected a directory at ${STATE_DIR}"
}

validate_repository_url() {
    local url="$1"
    local remainder=""
    local authority=""
    local repository_path=""
    local port=""

    # Only credential-free HTTPS repositories with a deliberately narrow
    # character set are supported. This also makes the URL safe to print in
    # diagnostics and in the copy/paste follow-up command.
    [[ "${url}" =~ ^https://[A-Za-z0-9.-]+(:[0-9]{1,5})?/[A-Za-z0-9._/-]+$ ]] ||
        die "--repo must be a credential-free HTTPS repository URL"
    remainder="${url#https://}"
    authority="${remainder%%/*}"
    repository_path="${remainder#*/}"
    [[ "${authority}" != *"@"* &&
        "${authority}" != .* &&
        "${authority}" != *..* &&
        "${authority}" != *.-* &&
        "${authority}" != *-.* &&
        "${repository_path}" != */ &&
        "${repository_path}" != *//* &&
        "${repository_path}" != ../* &&
        "${repository_path}" != *"/../"* &&
        "${repository_path}" != */.. &&
        "${repository_path}" != ./* &&
        "${repository_path}" != *"/./"* &&
        "${repository_path}" != */. ]] ||
        die "--repo contains an unsafe host or path"
    if [[ "${authority}" == *:* ]]; then
        port="${authority##*:}"
        ((10#${port} >= 1 && 10#${port} <= 65535)) ||
            die "--repo contains an invalid TCP port"
    fi
}

validate_inputs() {
    local config_mode=""
    local managed_config_path=""

    [[ "${CONFIG_PATH}" =~ ^/[A-Za-z0-9._/-]+$ ]] ||
        die "--config must be an absolute path without whitespace or shell metacharacters"
    [[ "${CONFIG_PATH}" != */ ]] || die "--config must name a file"
    [[ "${CONFIG_PATH}" != *//* &&
        "${CONFIG_PATH}" != *"/../"* &&
        "${CONFIG_PATH}" != *"/./"* &&
        "${CONFIG_PATH}" != */.. &&
        "${CONFIG_PATH}" != */. ]] ||
        die "--config must not contain empty, dot, or parent-directory components"
    CONFIG_DIR="${CONFIG_PATH%/*}"
    [[ -n "${CONFIG_DIR}" ]] || CONFIG_DIR="/"
    [[ "${CONFIG_DIR}" == "${CONFIG_ROOT}" ]] ||
        die "--config must be a direct child of the dedicated ${CONFIG_ROOT} directory"
    [[ "${CONFIG_PATH}" != "${CONFIG_ROOT}/token" ]] ||
        die "--config cannot overlap the managed token path"
    TOKEN_PATH="${CONFIG_DIR}/token"
    validate_trusted_directory_chain "${CONFIG_DIR}"
    if [[ -d "${CONFIG_DIR}" ]]; then
        [[ "$(stat -c '%u' "${CONFIG_DIR}")" == 0 ]] ||
            die "${CONFIG_DIR} must be owned by root"
        config_mode="$(stat -c '%a' "${CONFIG_DIR}")"
        (( (8#${config_mode} & 0022) == 0 )) ||
            die "${CONFIG_DIR} must not be writable by group or other users"
    fi
    for managed_config_path in "${CONFIG_PATH}" "${TOKEN_PATH}"; do
        [[ ! -L "${managed_config_path}" ]] ||
            die "refusing symbolic-link configuration file: ${managed_config_path}"
        [[ ! -e "${managed_config_path}" || -f "${managed_config_path}" ]] ||
            die "configuration path has an unexpected type: ${managed_config_path}"
        if [[ -f "${managed_config_path}" ]]; then
            [[ "$(stat -c '%u' "${managed_config_path}")" == 0 &&
                "$(stat -c '%h' "${managed_config_path}")" == 1 ]] ||
                die "configuration files must be single-link root-owned files: ${managed_config_path}"
            config_mode="$(stat -c '%a' "${managed_config_path}")"
            (( (8#${config_mode} & 0022) == 0 )) ||
                die "configuration files must not be writable by group or other users: ${managed_config_path}"
        fi
    done

    validate_repository_url "${REPO_URL}"
    [[ "${BRANCH}" =~ ^[A-Za-z0-9][A-Za-z0-9._/-]*$ ]] ||
        die "--branch contains unsupported characters"
    [[ "${BRANCH}" != *..* && "${BRANCH}" != */. && "${BRANCH}" != ./* ]] ||
        die "--branch is not a safe branch name"

    if [[ -n "${TOKEN_INPUT_FILE}" ]]; then
        [[ "${TOKEN_INPUT_FILE}" == /* ]] ||
            die "--token-file must be an absolute path"
    fi
    validate_managed_paths
}

validate_existing_token_before_build() {
    local token_mode=""
    local token_gid=""
    local service_gid=""
    local acl=""
    local acl_line=""

    [[ -f "${TOKEN_PATH}" ]] || return 0
    token_mode="$(stat -c '%a' "${TOKEN_PATH}")"
    (( (8#${token_mode} & 07137) == 0 &&
        (8#${token_mode} & 0400) != 0 )) ||
        die "${TOKEN_PATH} is readable outside its owner/service boundary; use root:root 0600 or root:${SERVICE_GROUP} 0640"

    if (( (8#${token_mode} & 0040) != 0 )); then
        validate_local_system_group "${SERVICE_GROUP}"
        service_gid="$(getent group "${SERVICE_GROUP}" | cut -d: -f3)"
        token_gid="$(stat -c '%g' "${TOKEN_PATH}")"
        [[ "${token_gid}" == "${service_gid}" ]] ||
            die "group-readable token ${TOKEN_PATH} must belong to ${SERVICE_GROUP}"
    fi

    command -v getfacl >/dev/null 2>&1 ||
        die "getfacl is required to verify token access controls"
    acl="$(getfacl --absolute-names --numeric --omit-header \
        "${TOKEN_PATH}" 2>/dev/null)" ||
        die "could not inspect access controls on ${TOKEN_PATH}"
    while IFS= read -r acl_line; do
        case "${acl_line}" in
            "" | user::[r-][w-][x-] | group::[r-][w-][x-] | other::[r-][w-][x-]) ;;
            *) die "${TOKEN_PATH} has an extended access ACL; remove it before building" ;;
        esac
    done <<<"${acl}"
}

confirm() {
    local prompt="$1"
    local answer=""

    if [[ "${NON_INTERACTIVE}" == true ]]; then
        return 1
    fi
    read_from_tty -p "${prompt} [y/N] " answer
    [[ "${answer}" =~ ^[Yy]([Ee][Ss])?$ ]]
}

detect_platform() {
    [[ -r /etc/os-release ]] || die "cannot identify this operating system"
    # shellcheck disable=SC1091
    . /etc/os-release

    local distro="${ID:-unknown}"
    local version="${VERSION_ID:-unknown}"
    local like="${ID_LIKE:-}"
    local major="${version%%.*}"

    case "${distro}" in
        debian)
            [[ "${major}" =~ ^[0-9]+$ && "${major}" -ge 12 ]] ||
                die "Debian 12 or newer is required (found ${version})"
            [[ "${major}" == 12 ]] ||
                warn "Debian ${version} is compatible but has not been formally tested"
            ;;
        ubuntu)
            case "${version}" in
                22.04 | 24.04) ;;
                *)
                    [[ "${major}" =~ ^[0-9]+$ && "${major}" -ge 22 ]] ||
                        die "Ubuntu 22.04 or newer is required (found ${version})"
                    warn "Ubuntu ${version} is compatible but has not been formally tested"
                    ;;
            esac
            ;;
        *)
            [[ " ${like} " == *" debian "* ]] ||
                die "only Debian, Ubuntu, and compatible Debian-based systems are supported"
            warn "${PRETTY_NAME:-${distro}} is treated as Debian-compatible but is not formally tested"
            ;;
    esac

    command -v dpkg >/dev/null 2>&1 || die "dpkg is required"
    local architecture
    architecture="$(dpkg --print-architecture)"
    case "${architecture}" in
        amd64 | arm64)
            log "Detected ${PRETTY_NAME:-${distro}} on ${architecture}"
            ;;
        *)
            die "unsupported architecture ${architecture}; supported: amd64, arm64"
            ;;
    esac
}

require_running_systemd() {
    command -v systemctl >/dev/null 2>&1 ||
        die "systemctl is required"
    command -v systemd-analyze >/dev/null 2>&1 ||
        die "systemd-analyze is required"
    [[ -d /run/systemd/system ]] ||
        die "systemd is not running; Cirrusync cannot be installed safely"
}

choose_action() {
    if [[ "${UNINSTALL_REQUESTED}" == true ]]; then
        ACTION="uninstall"
        return
    fi
    if [[ "${RECONFIGURE_REQUESTED}" == true ]]; then
        ACTION="reconfigure"
        return
    fi
    if [[ "${UPDATE_REQUESTED}" == true ]]; then
        ACTION="update"
        return
    fi

    if [[ ! -e "${BINARY_PATH}" && ! -e "${CONFIG_PATH}" && ! -e "${UNIT_PATH}" ]]; then
        ACTION="install"
        return
    fi

    if [[ "${NON_INTERACTIVE}" == true ]]; then
        if [[ ! -x "${BINARY_PATH}" || ! -f "${UNIT_PATH}" ||
            ! -f "${CONFIG_PATH}" || ! -f "${TOKEN_PATH}" ]]; then
            ACTION="install"
            INCOMPLETE_INSTALL=true
            log "Incomplete installation detected; rebuilding it transactionally"
        else
            ACTION="update"
            log "Existing installation detected; updating while preserving configuration and token"
        fi
        return
    fi

    cat <<'EOF'

An existing Cirrusync installation was detected.
  1) Update binary and reinstall unit; keep configuration and token
  2) Reconfigure; keep the installed binary
  3) Reinstall the systemd unit only
  4) Uninstall
  5) Exit
EOF
    local selection=""
    read_from_tty -p "Select an action [1]: " selection
    case "${selection:-1}" in
        1) ACTION="update" ;;
        2) ACTION="reconfigure" ;;
        3) ACTION="unit" ;;
        4) ACTION="uninstall" ;;
        5) ACTION="exit" ;;
        *) die "invalid selection" ;;
    esac
}

install_dependencies() {
    log "Installing build dependencies"
    export DEBIAN_FRONTEND=noninteractive
    apt-get update
    apt-get install --yes --no-install-recommends \
        acl \
        build-essential \
        ca-certificates \
        curl \
        git \
        pkg-config \
        procps \
        util-linux
}

rust_version_is_sufficient() {
    local rustc_path="$1"
    local version_output=""
    local major=""
    local minor=""

    version_output="$("${rustc_path}" --version 2>/dev/null)" || return 1
    [[ "${version_output}" =~ ^rustc[[:space:]]+([0-9]+)\.([0-9]+)\. ]] || return 1
    major="${BASH_REMATCH[1]}"
    minor="${BASH_REMATCH[2]}"
    ((major > 1 || (major == 1 && minor >= 85)))
}

trusted_executable() {
    local path="$1"
    local resolved=""
    local mode=""
    local parent=""
    local parent_mode=""

    resolved="$(readlink -f -- "${path}" 2>/dev/null)" || return 1
    [[ "${resolved}" == /usr/* || "${resolved}" == /opt/* ||
        "${resolved}" == "${BUILD_STATE_DIR}"/* ]] || return 1
    [[ -f "${resolved}" && -x "${resolved}" ]] || return 1
    [[ "$(stat -Lc '%u' "${resolved}")" == 0 ]] || return 1
    [[ "$(stat -Lc '%h' "${resolved}")" == 1 ]] || return 1
    mode="$(stat -Lc '%a' "${resolved}")"
    (( (8#${mode} & 06022) == 0 )) || return 1

    parent="$(dirname -- "${resolved}")"
    while [[ "${parent}" != / ]]; do
        [[ -d "${parent}" && ! -L "${parent}" ]] || return 1
        [[ "$(stat -Lc '%u' "${parent}")" == 0 ]] || return 1
        parent_mode="$(stat -Lc '%a' "${parent}")"
        (( (8#${parent_mode} & 0022) == 0 )) || return 1
        parent="$(dirname -- "${parent}")"
    done
}

system_toolchain_is_usable() {
    local cargo_path=""
    local rustc_path=""

    cargo_path="$(command -v cargo 2>/dev/null)" || return 1
    rustc_path="$(command -v rustc 2>/dev/null)" || return 1
    trusted_executable "${cargo_path}" || return 1
    trusted_executable "${rustc_path}" || return 1
    rust_version_is_sufficient "${rustc_path}" || return 1
    run_without_privilege_gain "${BUILD_USER}" \
        env -i \
        "HOME=/nonexistent" \
        "PATH=${TRUSTED_PATH}" \
        "RUSTC=${rustc_path}" \
        "${cargo_path}" --version >/dev/null 2>&1 || return 1
    run_without_privilege_gain "${BUILD_USER}" \
        env -i \
        "HOME=/nonexistent" \
        "PATH=${TRUSTED_PATH}" \
        "${rustc_path}" --version >/dev/null 2>&1 || return 1

    # Keep the invoked cargo path so a trusted rustup proxy still receives the
    # `cargo` argv[0] it uses for proxy dispatch. The unprivileged probe above
    # rejects per-user proxies that the isolated build account cannot use.
    CARGO_BIN="${cargo_path}"
    RUSTC_BIN="${rustc_path}"
}

install_managed_rust() {
    local architecture=""
    local rust_target=""
    local rustup_url=""
    local checksum_url=""
    local installer=""
    local checksum=""
    local rustup_bin="${BUILD_STATE_DIR}/cargo/bin/rustup"

    architecture="$(dpkg --print-architecture)"
    case "${architecture}" in
        amd64) rust_target="x86_64-unknown-linux-gnu" ;;
        arm64) rust_target="aarch64-unknown-linux-gnu" ;;
        *) die "cannot select a Rust target for ${architecture}" ;;
    esac

    install -d -o root -g root -m 0755 \
        "${BUILD_STATE_DIR}" \
        "${BUILD_STATE_DIR}/cargo" \
        "${BUILD_STATE_DIR}/rustup"

    if [[ -e "${rustup_bin}" || -L "${rustup_bin}" ]]; then
        trusted_executable "${rustup_bin}" ||
            die "managed rustup is not a trusted root-owned executable"
    fi

    if [[ ! -x "${rustup_bin}" ]]; then
        TEMP_DIR="$(mktemp -d /var/tmp/cirrusync-rustup.XXXXXX)"
        installer="${TEMP_DIR}/rustup-init"
        rustup_url="https://static.rust-lang.org/rustup/dist/${rust_target}/rustup-init"
        checksum_url="${rustup_url}.sha256"

        log "Downloading and verifying rustup-init for ${rust_target}"
        curl --fail --silent --show-error --location \
            --proto '=https' --tlsv1.2 "${rustup_url}" --output "${installer}"
        checksum="$(curl --fail --silent --show-error --location \
            --proto '=https' --tlsv1.2 "${checksum_url}")"
        checksum="${checksum%%[[:space:]]*}"
        [[ "${checksum}" =~ ^[0-9a-fA-F]{64}$ ]] ||
            die "rustup-init returned an invalid SHA-256 checksum"
        printf '%s  %s\n' "${checksum}" "${installer}" | sha256sum --check --status
        chmod 0755 "${installer}"

        log "Installing a managed stable Rust toolchain without changing shell profiles"
        CARGO_HOME="${BUILD_STATE_DIR}/cargo" \
            RUSTUP_HOME="${BUILD_STATE_DIR}/rustup" \
            "${installer}" -y --profile minimal --default-toolchain stable --no-modify-path
        rm -rf -- "${TEMP_DIR}"
        TEMP_DIR=""
    else
        log "Refreshing the managed stable Rust toolchain"
        CARGO_HOME="${BUILD_STATE_DIR}/cargo" \
            RUSTUP_HOME="${BUILD_STATE_DIR}/rustup" \
            "${rustup_bin}" set profile minimal
        CARGO_HOME="${BUILD_STATE_DIR}/cargo" \
            RUSTUP_HOME="${BUILD_STATE_DIR}/rustup" \
            "${rustup_bin}" toolchain install stable --profile minimal
    fi

    trusted_executable "${rustup_bin}" ||
        die "managed rustup installation is not a trusted root-owned executable"
    CARGO_BIN="$(CARGO_HOME="${BUILD_STATE_DIR}/cargo" \
        RUSTUP_HOME="${BUILD_STATE_DIR}/rustup" \
        "${rustup_bin}" which cargo --toolchain stable)"
    trusted_executable "${CARGO_BIN}" ||
        die "managed Cargo installation is not a trusted root-owned executable"
    RUSTC_BIN="$(CARGO_HOME="${BUILD_STATE_DIR}/cargo" \
        RUSTUP_HOME="${BUILD_STATE_DIR}/rustup" \
        "${rustup_bin}" which rustc --toolchain stable)"
    trusted_executable "${RUSTC_BIN}" ||
        die "managed rustc installation is not a trusted root-owned executable"
    chmod -R a+rX "${BUILD_STATE_DIR}/cargo" "${BUILD_STATE_DIR}/rustup"
}

ensure_rust() {
    if system_toolchain_is_usable; then
        log "Using existing Rust toolchain: $("${CARGO_BIN}" --version)"
        return
    fi

    install_managed_rust
    log "Using managed Rust toolchain: $("${CARGO_BIN}" --version)"
}

verify_existing_repository_is_recoverable() {
    local old_tree="$1"
    local new_tree="$2"
    local new_home="$3"
    local old_head=""
    local old_commits=""
    local new_commits=""
    local old_refs=""
    local ref_name=""
    local old_object=""
    local new_object=""
    local object=""
    local -A new_commit_set=()
    local -a old_environment=(
        "HOME=/nonexistent"
        "PATH=/usr/bin:/bin"
        "GIT_CONFIG_NOSYSTEM=1"
        "GIT_CONFIG_GLOBAL=${GIT_SAFE_CONFIG}"
        "GIT_NO_REPLACE_OBJECTS=1"
        "GIT_OPTIONAL_LOCKS=0"
        "GIT_TERMINAL_PROMPT=0"
    )
    local -a new_environment=(
        "HOME=${new_home}"
        "PATH=/usr/bin:/bin"
        "GIT_CONFIG_NOSYSTEM=1"
        "GIT_CONFIG_GLOBAL=/dev/null"
        "GIT_NO_REPLACE_OBJECTS=1"
        "GIT_OPTIONAL_LOCKS=0"
        "GIT_TERMINAL_PROMPT=0"
    )

    old_head="$(run_without_privilege_gain "${BUILD_USER}" \
        env -i "${old_environment[@]}" \
        git -c core.hooksPath=/dev/null -C "${old_tree}" rev-parse --verify HEAD)"
    run_without_privilege_gain "${BUILD_USER}" \
        env -i "${new_environment[@]}" \
        git -c core.hooksPath=/dev/null -C "${new_tree}" \
        merge-base --is-ancestor "${old_head}" HEAD ||
        die "the managed checkout contains history absent from the requested branch; move it aside before updating"

    new_commits="$(run_without_privilege_gain "${BUILD_USER}" \
        env -i "${new_environment[@]}" \
        git -c core.hooksPath=/dev/null -C "${new_tree}" rev-list --all)"
    while IFS= read -r object; do
        [[ -z "${object}" ]] && continue
        [[ "${object}" =~ ^[0-9a-f]{40,64}$ ]] ||
            die "the staged checkout returned an invalid commit identifier"
        new_commit_set["${object}"]=1
    done <<<"${new_commits}"

    old_commits="$(run_without_privilege_gain "${BUILD_USER}" \
        env -i "${old_environment[@]}" \
        git -c core.hooksPath=/dev/null -C "${old_tree}" rev-list --all --reflog)"
    while IFS= read -r object; do
        [[ -z "${object}" ]] && continue
        [[ "${object}" =~ ^[0-9a-f]{40,64}$ ]] ||
            die "the managed checkout returned an invalid commit identifier"
        [[ -n "${new_commit_set[${object}]:-}" ]] ||
            die "the managed checkout has local or reflog-only commits; move it aside before updating"
    done <<<"${old_commits}"

    old_refs="$(run_without_privilege_gain "${BUILD_USER}" \
        env -i "${old_environment[@]}" \
        git -c core.hooksPath=/dev/null -C "${old_tree}" \
        for-each-ref --format='%(refname)%09%(objectname)' refs)"
    while IFS=$'\t' read -r ref_name old_object; do
        [[ -n "${ref_name}" ]] || continue
        [[ "${ref_name}" == refs/* &&
            "${old_object}" =~ ^[0-9a-f]{40,64}$ ]] ||
            die "the managed checkout returned invalid reference metadata"
        case "${ref_name}" in
            "refs/heads/${BRANCH}" | "refs/remotes/origin/${BRANCH}" | \
                refs/remotes/origin/HEAD)
                run_without_privilege_gain "${BUILD_USER}" \
                    env -i "${new_environment[@]}" \
                    git -c core.hooksPath=/dev/null -C "${new_tree}" \
                    merge-base --is-ancestor "${old_object}" HEAD ||
                    die "the managed checkout has a divergent ${ref_name}; move it aside before updating"
                ;;
            *)
                new_object="$(run_without_privilege_gain "${BUILD_USER}" \
                    env -i "${new_environment[@]}" \
                    git -c core.hooksPath=/dev/null -C "${new_tree}" \
                    show-ref --verify --hash "${ref_name}" 2>/dev/null || true)"
                [[ "${new_object}" == "${old_object}" ]] ||
                    die "the managed checkout has a local ref ${ref_name}; move it aside before updating"
                ;;
        esac
    done <<<"${old_refs}"
}

sync_source() {
    local current_origin=""
    local status_output=""
    local ignored_output=""
    local index_flag_output=""
    local source_home=""
    local -a git_environment=()

    if [[ ! -d "${SOURCE_DIR%/*}" ]]; then
        install -d -o root -g root -m 0755 "${SOURCE_DIR%/*}"
    fi
    validate_trusted_directory_chain "${SOURCE_DIR%/*}"
    git_environment=(
        "HOME=/nonexistent"
        "PATH=/usr/bin:/bin"
        "GIT_CONFIG_NOSYSTEM=1"
        "GIT_NO_REPLACE_OBJECTS=1"
        "GIT_OPTIONAL_LOCKS=0"
        "GIT_TERMINAL_PROMPT=0"
    )

    [[ ! -L "${SOURCE_DIR}" ]] ||
        die "refusing to use a symbolic link as the managed source directory"

    if [[ -e "${SOURCE_DIR}" ]]; then
        [[ -d "${SOURCE_DIR}/.git" ]] ||
            die "${SOURCE_DIR} exists but is not a managed Git checkout"

        # Git only honors safe.directory in protected configuration on the
        # older Git versions shipped by supported distributions. Put the one
        # trusted path in a root-owned global config, then inspect repository
        # metadata as the unprivileged build account. Local settings such as a
        # hostile core.fsmonitor can therefore never execute as root.
        GIT_SAFE_CONFIG="$(mktemp "${SOURCE_DIR%/*}/.cirrusync-git-config.XXXXXX")"
        printf '[safe]\n\tdirectory = %s\n' "${SOURCE_DIR}" >"${GIT_SAFE_CONFIG}"
        chown "root:${BUILD_GROUP}" "${GIT_SAFE_CONFIG}"
        chmod 0640 "${GIT_SAFE_CONFIG}"

        current_origin="$(run_without_privilege_gain "${BUILD_USER}" \
            env -i "${git_environment[@]}" \
            "GIT_CONFIG_GLOBAL=${GIT_SAFE_CONFIG}" \
            git \
            -c core.hooksPath=/dev/null \
            -C "${SOURCE_DIR}" remote get-url origin)"
        [[ "${current_origin}" == "${REPO_URL}" ]] ||
            die "${SOURCE_DIR} has a different origin; move it aside or use the matching credential-free --repo URL"

        status_output="$(run_without_privilege_gain "${BUILD_USER}" \
            env -i "${git_environment[@]}" \
            "GIT_CONFIG_GLOBAL=${GIT_SAFE_CONFIG}" \
            git \
            -c core.hooksPath=/dev/null \
            -C "${SOURCE_DIR}" status --porcelain=v1 --untracked-files=all)"
        [[ -z "${status_output}" ]] ||
            die "${SOURCE_DIR} has local changes; move it aside before updating"
        ignored_output="$(run_without_privilege_gain "${BUILD_USER}" \
            env -i "${git_environment[@]}" \
            "GIT_CONFIG_GLOBAL=${GIT_SAFE_CONFIG}" \
            git \
            -c core.hooksPath=/dev/null \
            -C "${SOURCE_DIR}" ls-files --others --ignored --exclude-standard)"
        [[ -z "${ignored_output}" ]] ||
            die "${SOURCE_DIR} contains ignored files; move them before updating"
        index_flag_output="$(run_without_privilege_gain "${BUILD_USER}" \
            env -i "${git_environment[@]}" \
            "GIT_CONFIG_GLOBAL=${GIT_SAFE_CONFIG}" \
            git \
            -c core.hooksPath=/dev/null \
            -C "${SOURCE_DIR}" ls-files -v |
            awk 'substr($0, 1, 2) ~ /^([a-z]|S) / { print; exit }')"
        [[ -z "${index_flag_output}" ]] ||
            die "${SOURCE_DIR} uses assume-unchanged or skip-worktree flags; clear them before updating"

        terminate_account_processes "${BUILD_USER}" ||
            die "managed source inspection left processes that could not be terminated"
    fi

    SOURCE_STAGE_ROOT="$(mktemp -d "${SOURCE_DIR%/*}/.cirrusync-source.XXXXXX")"
    chown "${BUILD_USER}:${BUILD_GROUP}" "${SOURCE_STAGE_ROOT}"
    chmod 0700 "${SOURCE_STAGE_ROOT}"
    source_home="${SOURCE_STAGE_ROOT}/home"
    install -d -o "${BUILD_USER}" -g "${BUILD_GROUP}" -m 0700 "${source_home}"
    git_environment[0]="HOME=${source_home}"
    STAGED_SOURCE="${SOURCE_STAGE_ROOT}/source"
    log "Cloning ${REPO_URL} (${BRANCH}) into a staged source tree"
    run_without_privilege_gain "${BUILD_USER}" env -i "${git_environment[@]}" \
        "GIT_CONFIG_GLOBAL=/dev/null" \
        git \
        -c core.hooksPath=/dev/null \
        -c protocol.file.allow=never \
        -c protocol.ext.allow=never \
        -c http.followRedirects=false \
        clone --branch "${BRANCH}" --single-branch -- \
        "${REPO_URL}" "${STAGED_SOURCE}"
    [[ -f "${STAGED_SOURCE}/Cargo.lock" &&
        -f "${STAGED_SOURCE}/systemd/cirrusync.service" ]] ||
        die "the staged source is missing required project files"
    if [[ -e "${SOURCE_DIR}" ]]; then
        verify_existing_repository_is_recoverable \
            "${SOURCE_DIR}" "${STAGED_SOURCE}" "${source_home}"
        terminate_account_processes "${BUILD_USER}" ||
            die "repository history inspection left processes that could not be terminated"
        rm -f -- "${GIT_SAFE_CONFIG}"
        GIT_SAFE_CONFIG=""
    fi
}

build_project() {
    local cargo_home=""
    local target_dir=""
    local home_dir=""
    local status_output=""
    local ignored_output=""
    local index_flag_output=""
    local unexpected_entry=""
    local -a cargo_environment=()

    create_build_account
    trusted_executable "${CARGO_BIN}" ||
        die "Cargo changed after toolchain validation"
    trusted_executable "${RUSTC_BIN}" ||
        die "rustc changed after toolchain validation"
    BUILD_DIR="$(mktemp -d /var/tmp/cirrusync-build.XXXXXX)"
    cargo_home="${BUILD_DIR}/cargo-home"
    target_dir="${BUILD_DIR}/target"
    home_dir="${BUILD_DIR}/home"
    install -d -o "${BUILD_USER}" -g "${BUILD_GROUP}" -m 0700 \
        "${cargo_home}" "${target_dir}" "${home_dir}"
    chown "${BUILD_USER}:${BUILD_GROUP}" "${BUILD_DIR}"
    chmod 0700 "${BUILD_DIR}"

    cargo_environment=(
        "HOME=${home_dir}"
        "CARGO_HOME=${cargo_home}"
        "CARGO_TARGET_DIR=${target_dir}"
        "RUSTC=${RUSTC_BIN}"
        "PATH=$(dirname "${CARGO_BIN}"):/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
    )

    log "Building the locked release as the dedicated ${BUILD_USER} account"
    run_without_privilege_gain "${BUILD_USER}" env -i "${cargo_environment[@]}" \
        "${CARGO_BIN}" build --release --locked \
        --manifest-path "${STAGED_SOURCE}/Cargo.toml"
    terminate_account_processes "${BUILD_USER}" ||
        die "the release build left processes that could not be terminated"

    if [[ "${SKIP_TESTS}" == true ]]; then
        warn "Tests were skipped at the operator's request"
    else
        log "Running the locked test suite as the dedicated ${BUILD_USER} account"
        run_without_privilege_gain "${BUILD_USER}" env -i "${cargo_environment[@]}" \
            "${CARGO_BIN}" test --locked \
            --manifest-path "${STAGED_SOURCE}/Cargo.toml"
        terminate_account_processes "${BUILD_USER}" ||
            die "the test suite left processes that could not be terminated"
    fi

    [[ -f "${target_dir}/release/cirrusync" &&
        ! -L "${target_dir}/release/cirrusync" ]] ||
        die "the release build did not produce ${target_dir}/release/cirrusync"
    [[ -d "${STAGED_SOURCE}" && ! -L "${STAGED_SOURCE}" &&
        "$(readlink -f -- "${STAGED_SOURCE}")" == "${STAGED_SOURCE}" ]] ||
        die "the build replaced its staged source root with an unsafe path"
    status_output="$(run_without_privilege_gain "${BUILD_USER}" \
        env -i \
        "HOME=${home_dir}" \
        "PATH=/usr/bin:/bin" \
        "GIT_CONFIG_NOSYSTEM=1" \
        "GIT_CONFIG_GLOBAL=/dev/null" \
        "GIT_NO_REPLACE_OBJECTS=1" \
        "GIT_OPTIONAL_LOCKS=0" \
        git -c core.hooksPath=/dev/null \
        -C "${STAGED_SOURCE}" status --porcelain=v1 --untracked-files=all)"
    [[ -z "${status_output}" ]] ||
        die "the build or tests modified the staged source tree"
    ignored_output="$(run_without_privilege_gain "${BUILD_USER}" \
        env -i \
        "HOME=${home_dir}" \
        "PATH=/usr/bin:/bin" \
        "GIT_CONFIG_NOSYSTEM=1" \
        "GIT_CONFIG_GLOBAL=/dev/null" \
        "GIT_NO_REPLACE_OBJECTS=1" \
        "GIT_OPTIONAL_LOCKS=0" \
        git -c core.hooksPath=/dev/null \
        -C "${STAGED_SOURCE}" ls-files --others --ignored --exclude-standard)"
    [[ -z "${ignored_output}" ]] ||
        die "the build or tests left ignored files in the staged source tree"
    index_flag_output="$(run_without_privilege_gain "${BUILD_USER}" \
        env -i \
        "HOME=${home_dir}" \
        "PATH=/usr/bin:/bin" \
        "GIT_CONFIG_NOSYSTEM=1" \
        "GIT_CONFIG_GLOBAL=/dev/null" \
        "GIT_NO_REPLACE_OBJECTS=1" \
        "GIT_OPTIONAL_LOCKS=0" \
        git -c core.hooksPath=/dev/null \
        -C "${STAGED_SOURCE}" ls-files -v |
        awk 'substr($0, 1, 2) ~ /^([a-z]|S) / { print; exit }')"
    [[ -z "${index_flag_output}" ]] ||
        die "the build or tests set unsafe Git index flags"
    terminate_account_processes "${BUILD_USER}" ||
        die "post-build source inspection left processes that could not be terminated"
    unexpected_entry="$(find "${STAGED_SOURCE}" -xdev \
        ! -type f ! -type d ! -type l -print -quit)"
    [[ -z "${unexpected_entry}" ]] ||
        die "the staged source contains a special file"
    unexpected_entry="$(find "${STAGED_SOURCE}" -xdev \
        -type f -links +1 -print -quit)"
    [[ -z "${unexpected_entry}" ]] ||
        die "the staged source contains a hard-linked file"
    chown --recursive --no-dereference root:root "${STAGED_SOURCE}"
    chmod --recursive u+rwX,go+rX,go-w "${STAGED_SOURCE}"

    if [[ ! -d "${BINARY_PATH%/*}" ]]; then
        install -d -o root -g root -m 0755 "${BINARY_PATH%/*}"
    fi
    validate_trusted_directory_chain "${BINARY_PATH%/*}"
    STAGED_BINARY="$(mktemp "/usr/local/bin/.cirrusync.XXXXXX")"
    install -o root -g root -m 0755 \
        "${target_dir}/release/cirrusync" "${STAGED_BINARY}"
}

validate_local_system_group() {
    local group_name="$1"
    local local_entry=""
    local resolved_entry=""
    local parsed_name=""
    local _password=""
    local gid=""
    local members=""
    local primary_users=""
    local shadow_entry=""
    local shadow_password=""
    local shadow_admins=""
    local shadow_members=""
    local system_gid_max="999"

    local_entry="$(awk -F: -v name="${group_name}" \
        '$1 == name { print }' /etc/group)"
    resolved_entry="$(getent group "${group_name}" || true)"
    [[ -n "${local_entry}" && "${resolved_entry}" == "${local_entry}" &&
        "${local_entry}" != *$'\n'* ]] ||
        die "${group_name} must be a single local system group"
    IFS=: read -r parsed_name _password gid members <<<"${local_entry}"
    system_gid_max="$(awk \
        '$1 == "SYS_GID_MAX" && $2 ~ /^[0-9]+$/ { value=$2 } END { if (value) print value }' \
        /etc/login.defs)"
    system_gid_max="${system_gid_max:-999}"
    [[ "${parsed_name}" == "${group_name}" && "${gid}" =~ ^[0-9]+$ &&
        "${gid}" -ne 0 && "${gid}" -le "${system_gid_max}" ]] ||
        die "${group_name} has an unsafe group identity"
    [[ "$(awk -F: -v id="${gid}" '$3 == id { count++ } END { print count+0 }' \
        /etc/group)" == 1 ]] ||
        die "${group_name} shares its numeric GID with another local group"
    [[ -z "${members}" ]] ||
        die "${group_name} must not contain explicit group members"
    primary_users="$(awk -F: -v id="${gid}" '$4 == id { print $1 }' /etc/passwd)"
    [[ -z "${primary_users}" || "${primary_users}" == "${group_name}" ]] ||
        die "${group_name} is the primary group of an unexpected local user"
    shadow_entry="$(awk -F: -v name="${group_name}" \
        '$1 == name { print }' /etc/gshadow)"
    [[ -n "${shadow_entry}" && "${shadow_entry}" != *$'\n'* &&
        "$(getent gshadow "${group_name}" || true)" == "${shadow_entry}" ]] ||
        die "${group_name} must have one local shadow-group entry"
    IFS=: read -r parsed_name shadow_password shadow_admins shadow_members \
        <<<"${shadow_entry}"
    [[ "${parsed_name}" == "${group_name}" &&
        ( "${shadow_password}" == '!'* || "${shadow_password}" == '*'* ) &&
        -z "${shadow_admins}" && -z "${shadow_members}" ]] ||
        die "${group_name} must have a locked password and no administrators or members"
}

validate_local_system_user() {
    local user_name="$1"
    local group_name="$2"
    local expected_home="$3"
    local local_entry=""
    local resolved_entry=""
    local parsed_name=""
    local _password=""
    local uid=""
    local gid=""
    local _gecos=""
    local home=""
    local shell=""
    local expected_gid=""
    local shadow_entry=""
    local shadow_password=""
    local system_uid_max="999"
    local group_ids=""

    local_entry="$(awk -F: -v name="${user_name}" \
        '$1 == name { print }' /etc/passwd)"
    resolved_entry="$(getent passwd "${user_name}" || true)"
    [[ -n "${local_entry}" && "${resolved_entry}" == "${local_entry}" &&
        "${local_entry}" != *$'\n'* ]] ||
        die "${user_name} must be a single local system user"
    IFS=: read -r parsed_name _password uid gid _gecos home shell <<<"${local_entry}"
    expected_gid="$(getent group "${group_name}" | cut -d: -f3)"
    system_uid_max="$(awk \
        '$1 == "SYS_UID_MAX" && $2 ~ /^[0-9]+$/ { value=$2 } END { if (value) print value }' \
        /etc/login.defs)"
    system_uid_max="${system_uid_max:-999}"
    [[ "${parsed_name}" == "${user_name}" && "${uid}" =~ ^[0-9]+$ &&
        "${uid}" -ne 0 && "${uid}" -le "${system_uid_max}" ]] ||
        die "${user_name} is not a safe system user"
    [[ "$(awk -F: -v id="${uid}" '$3 == id { count++ } END { print count+0 }' \
        /etc/passwd)" == 1 ]] ||
        die "${user_name} shares its numeric UID with another local user"
    [[ "${gid}" == "${expected_gid}" ]] ||
        die "${user_name} must use ${group_name} as its primary group"
    [[ "${home}" == "${expected_home}" ]] ||
        die "${user_name} has unexpected home directory ${home}"
    [[ "${shell}" == "/usr/sbin/nologin" || "${shell}" == "/bin/false" ]] ||
        die "${user_name} must use a non-login shell"
    group_ids="$(id -G "${user_name}")"
    [[ "${group_ids}" == "${expected_gid}" ]] ||
        die "${user_name} must not belong to supplementary groups"
    shadow_entry="$(getent shadow "${user_name}" || true)"
    [[ -n "${shadow_entry}" ]] ||
        die "cannot verify that ${user_name} has a locked password"
    IFS=: read -r parsed_name shadow_password _ <<<"${shadow_entry}"
    [[ "${parsed_name}" == "${user_name}" &&
        ( "${shadow_password}" == '!'* || "${shadow_password}" == '*'* ) ]] ||
        die "${user_name} must have a locked password"
}

ensure_system_identity() {
    local user_name="$1"
    local group_name="$2"
    local home="$3"
    local comment="$4"
    local nologin_shell="/usr/sbin/nologin"

    if id "${user_name}" >/dev/null 2>&1; then
        getent group "${group_name}" >/dev/null 2>&1 ||
            die "${user_name} exists but ${group_name} does not"
    else
        if getent group "${group_name}" >/dev/null 2>&1; then
            validate_local_system_group "${group_name}"
        else
            groupadd --system "${group_name}"
        fi
        [[ -x "${nologin_shell}" ]] || nologin_shell="/bin/false"
        useradd --system \
            --gid "${group_name}" \
            --home-dir "${home}" \
            --shell "${nologin_shell}" \
            --comment "${comment}" \
            "${user_name}"
    fi

    validate_local_system_group "${group_name}"
    validate_local_system_user "${user_name}" "${group_name}" "${home}"
}

create_build_account() {
    ensure_system_identity \
        "${BUILD_USER}" "${BUILD_GROUP}" "${BUILD_STATE_DIR}" \
        "Cirrusync isolated build account"
    BUILD_ACCOUNT_VALIDATED=true
    terminate_account_processes "${BUILD_USER}" ||
        die "could not clear processes owned by the dedicated ${BUILD_USER} account"
}

create_service_account() {
    local state_uid=""
    local state_gid=""
    local service_uid=""
    local service_gid=""

    ensure_system_identity \
        "${SERVICE_USER}" "${SERVICE_GROUP}" "${STATE_DIR}" \
        "Cirrusync service account"
    SERVICE_ACCOUNT_VALIDATED=true

    [[ ! -L "${CONFIG_DIR}" ]] ||
        die "refusing to use a symbolic link as the configuration directory"
    [[ ! -L "${STATE_DIR}" ]] ||
        die "refusing to use a symbolic link as the state directory"
    assert_directory_chain_is_safe "${CONFIG_DIR}"
    assert_directory_chain_is_safe "${STATE_DIR}"
    if [[ -d "${STATE_DIR}" ]]; then
        state_uid="$(stat -c '%u' "${STATE_DIR}")"
        state_gid="$(stat -c '%g' "${STATE_DIR}")"
        service_uid="$(id -u "${SERVICE_USER}")"
        service_gid="$(getent group "${SERVICE_GROUP}" | cut -d: -f3)"
        [[ "${state_uid}" == "${service_uid}" &&
            "${state_gid}" == "${service_gid}" ]] ||
            die "${STATE_DIR} has unexpected ownership; refusing to change it"
    fi
    install -d -o root -g "${SERVICE_GROUP}" -m 0750 "${CONFIG_DIR}"
    install -d -o "${SERVICE_USER}" -g "${SERVICE_GROUP}" -m 0750 "${STATE_DIR}"
}

validate_configuration_directory_acl() {
    local directory_acl=""
    local acl_line=""

    [[ -d "${CONFIG_DIR}" ]] || return 0
    command -v getfacl >/dev/null 2>&1 ||
        die "getfacl is required to verify configuration directory access controls"
    directory_acl="$(getfacl --absolute-names --numeric --omit-header \
        "${CONFIG_DIR}" 2>/dev/null)" ||
        die "could not inspect access controls on ${CONFIG_DIR}"
    while IFS= read -r acl_line; do
        case "${acl_line}" in
            "" | user::[r-][w-][x-] | group::[r-][w-][x-] | other::[r-][w-][x-]) ;;
            *)
                die "${CONFIG_DIR} has an extended or default ACL; remove it before installing secrets"
                ;;
        esac
    done <<<"${directory_acl}"
    return 0
}

secure_existing_configuration_files() {
    local path=""

    # Check again after the service has stopped so a directory ACL changed
    # between preflight validation and the transaction cannot expose secrets.
    validate_configuration_directory_acl

    for path in "${CONFIG_PATH}" "${TOKEN_PATH}"; do
        if [[ -e "${path}" || -L "${path}" ]]; then
            [[ -f "${path}" && ! -L "${path}" &&
                "$(stat -c '%h' "${path}")" == 1 ]] ||
                die "managed configuration files must be regular, non-linked files: ${path}"
            command -v setfacl >/dev/null 2>&1 ||
                die "setfacl is required to secure managed configuration files"
            setfacl --remove-all "${path}"
            chown root:"${SERVICE_GROUP}" "${path}"
            chmod 0640 "${path}"
        fi
    done
}

normalize_boolean() {
    local value="${1,,}"
    case "${value}" in
        1 | true | yes | y | on) printf 'true\n' ;;
        0 | false | no | n | off) printf 'false\n' ;;
        *) return 1 ;;
    esac
}

prompt_value() {
    local prompt="$1"
    local default_value="$2"
    local supplied_value="$3"
    local result=""

    if [[ -n "${supplied_value}" ]]; then
        printf '%s\n' "${supplied_value}"
        return
    fi
    if [[ "${NON_INTERACTIVE}" == true ]]; then
        printf '%s\n' "${default_value}"
        return
    fi

    if [[ -n "${default_value}" ]]; then
        read_from_tty -p "${prompt} [${default_value}]: " result
        printf '%s\n' "${result:-${default_value}}"
    else
        read_from_tty -p "${prompt}: " result
        printf '%s\n' "${result}"
    fi
}

prompt_boolean() {
    local prompt="$1"
    local default_value="$2"
    local supplied_value="$3"
    local result=""
    local hint="y/N"

    if [[ -n "${supplied_value}" ]]; then
        normalize_boolean "${supplied_value}" ||
            die "${prompt} must be true or false"
        return
    fi
    if [[ "${NON_INTERACTIVE}" == true ]]; then
        printf '%s\n' "${default_value}"
        return
    fi

    [[ "${default_value}" == true ]] && hint="Y/n"
    read_from_tty -p "${prompt} [${hint}]: " result
    if [[ -z "${result}" ]]; then
        printf '%s\n' "${default_value}"
    else
        normalize_boolean "${result}" ||
            die "${prompt} must be yes or no"
    fi
}

validate_dns_name() {
    local name="${1%.}"
    local label=""
    local -a labels=()

    [[ -n "${name}" && ${#name} -le 253 ]] || return 1
    IFS='.' read -r -a labels <<<"${name}"
    ((${#labels[@]} >= 2)) || return 1
    for label in "${labels[@]}"; do
        [[ ${#label} -ge 1 && ${#label} -le 63 ]] || return 1
        [[ "${label}" =~ ^[A-Za-z0-9]([A-Za-z0-9-]{0,61}[A-Za-z0-9])?$ ]] ||
            return 1
    done
}

write_token() {
    local token_source="${TOKEN_INPUT_FILE}"
    local environment_token="${INPUT_TOKEN_VALUE}"
    local temporary_token=""
    local entered_token=""
    local mode=""
    local token_fd=""
    local descriptor_path=""
    local descriptor_identity=""
    local source_identity=""
    local token_size=""
    local token_source_parent=""
    local token_owner=""
    local token_links=""
    local replace_existing=false

    if [[ -z "${token_source}" ]]; then
        token_source="${INPUT_TOKEN_FILE}"
    fi

    if [[ -e "${TOKEN_PATH}" || -L "${TOKEN_PATH}" ]]; then
        [[ -f "${TOKEN_PATH}" && ! -L "${TOKEN_PATH}" ]] ||
            die "the existing token path must be a regular, non-symlink file"
        chown root:"${SERVICE_GROUP}" "${TOKEN_PATH}"
        chmod 0640 "${TOKEN_PATH}"
        if [[ "${NON_INTERACTIVE}" == true ]]; then
            if [[ "${ACTION}" == reconfigure &&
                ( -n "${token_source}" || -n "${environment_token}" ) ]]; then
                replace_existing=true
                log "Replacing the token from explicit reconfiguration input"
            elif [[ "${INCOMPLETE_INSTALL}" == true &&
                ( -n "${token_source}" || -n "${environment_token}" ) ]]; then
                replace_existing=true
                log "Replacing the token from explicit recovery input"
            else
                [[ -z "${token_source}" && -z "${environment_token}" ]] ||
                    warn "Existing token preserved; supplied token input was ignored"
                return
            fi
        fi
        if [[ "${replace_existing}" == false ]] &&
            ! confirm "Replace the existing token at ${TOKEN_PATH}?"; then
            log "Keeping the existing token"
            return
        fi
    fi

    temporary_token="$(mktemp "${CONFIG_DIR}/.token.XXXXXX")"
    TOKEN_TEMP="${temporary_token}"
    chmod 0600 "${temporary_token}"

    if [[ -n "${token_source}" ]]; then
        [[ "${token_source}" == /* && ! -L "${token_source}" ]] ||
            die "token source must be an absolute, non-symlink path"
        token_source_parent="$(dirname -- "${token_source}")"
        validate_trusted_directory_chain "${token_source_parent}"
        [[ -f "${token_source}" ]] ||
            die "token source must resolve to a regular file"
        exec {token_fd}<"${token_source}" ||
            die "cannot open token source file: ${token_source}"
        descriptor_path="/proc/${BASHPID}/fd/${token_fd}"
        [[ -f "${descriptor_path}" ]] ||
            die "token source must resolve to a regular file"
        descriptor_identity="$(stat -Lc '%d:%i' "${descriptor_path}")"
        source_identity="$(stat -Lc '%d:%i' "${token_source}" 2>/dev/null || true)"
        [[ ! -L "${token_source}" &&
            "${source_identity}" == "${descriptor_identity}" ]] ||
            die "token source changed while it was being opened"
        token_size="$(stat -Lc '%s' "${descriptor_path}")"
        [[ "${token_size}" =~ ^[0-9]+$ && "${token_size}" -gt 0 &&
            "${token_size}" -le 16384 ]] ||
            die "token source must contain between 1 and 16384 bytes"
        token_owner="$(stat -Lc '%u' "${descriptor_path}")"
        token_links="$(stat -Lc '%h' "${descriptor_path}")"
        mode="$(stat -Lc '%a' "${descriptor_path}" 2>/dev/null || true)"
        [[ "${token_owner}" == 0 && "${token_links}" == 1 ]] ||
            die "token source must be root-owned and have exactly one hard link"
        [[ "${mode}" =~ ^[0-7]{3,4}$ ]] ||
            die "cannot determine secure token-source permissions"
        (( (8#${mode} & 0400) != 0 && (8#${mode} & 0177) == 0 )) ||
            die "token source must grant root read access and no group/other access (use mode 0600)"
        dd if="${descriptor_path}" of="${temporary_token}" \
            bs=16385 count=1 iflag=fullblock status=none
        exec {token_fd}<&-
        token_size="$(stat -c '%s' "${temporary_token}")"
        [[ "${token_size}" -gt 0 && "${token_size}" -le 16384 ]] ||
            die "token source changed size while it was being copied"
        chown root:"${SERVICE_GROUP}" "${temporary_token}"
        chmod 0640 "${temporary_token}"
    elif [[ -n "${environment_token}" ]]; then
        warn "Reading a token from the environment; --token-file is safer"
        [[ "${environment_token}" != *$'\n'* && "${environment_token}" != *$'\r'* ]] ||
            die "the environment token contains a newline"
        printf '%s\n' "${environment_token}" >"${temporary_token}"
        chown root:"${SERVICE_GROUP}" "${temporary_token}"
        chmod 0640 "${temporary_token}"
    elif [[ "${NON_INTERACTIVE}" == true ]]; then
        rm -f -- "${temporary_token}"
        die "set CIRRUSYNC_TOKEN_FILE or CIRRUSYNC_TOKEN for a new non-interactive install"
    else
        read_from_tty -s -p "Cloudflare API token: " entered_token
        printf '\n'
        [[ -n "${entered_token}" ]] || die "the Cloudflare API token cannot be empty"
        [[ "${entered_token}" != *$'\n'* && "${entered_token}" != *$'\r'* ]] ||
            die "the token contains a newline"
        printf '%s\n' "${entered_token}" >"${temporary_token}"
        unset entered_token
        chown root:"${SERVICE_GROUP}" "${temporary_token}"
        chmod 0640 "${temporary_token}"
    fi

    mv -f -- "${temporary_token}" "${TOKEN_PATH}"
    TOKEN_TEMP=""
}

write_configuration() {
    local zone=""
    local record=""
    local ipv4=""
    local ipv6=""
    local interval=""
    local create_missing=""
    local proxied=""
    local temporary_config=""
    local zone_lower=""
    local record_lower=""
    local env_zone="${INPUT_ZONE}"
    local env_record="${INPUT_RECORD}"
    local env_ipv4="${INPUT_IPV4}"
    local env_ipv6="${INPUT_IPV6}"
    local env_interval="${INPUT_INTERVAL}"
    local env_create="${INPUT_CREATE}"
    local env_proxied="${INPUT_PROXIED}"
    local record_ttl="120"

    if [[ -e "${CONFIG_PATH}" || -L "${CONFIG_PATH}" ]]; then
        [[ -f "${CONFIG_PATH}" && ! -L "${CONFIG_PATH}" ]] ||
            die "the existing configuration path must be a regular, non-symlink file"
        if [[ "${ACTION}" != reconfigure ]]; then
            if [[ "${INCOMPLETE_INSTALL}" == true &&
                ( -n "${env_zone}" || -n "${env_record}" ||
                    -n "${env_ipv4}" || -n "${env_ipv6}" ||
                    -n "${env_interval}" || -n "${env_create}" ||
                    -n "${env_proxied}" ) ]]; then
                log "Replacing the incomplete configuration from explicit recovery input"
            else
                chown root:"${SERVICE_GROUP}" "${CONFIG_PATH}"
                chmod 0640 "${CONFIG_PATH}"
                if [[ "${INCOMPLETE_INSTALL}" == true &&
                    ( -n "${TOKEN_INPUT_FILE}" || -n "${INPUT_TOKEN_FILE}" ||
                        -n "${INPUT_TOKEN_VALUE}" ) ]]; then
                    write_token
                fi
                log "Keeping existing configuration at ${CONFIG_PATH}"
                return
            fi
        elif [[ "${NON_INTERACTIVE}" == false ]]; then
            confirm "Replace the existing configuration at ${CONFIG_PATH}?" ||
                die "reconfiguration cancelled; the existing file was not changed"
        fi
    fi

    zone="$(prompt_value "Cloudflare zone" "" "${env_zone}")"
    record="$(prompt_value "DNS record name" "" "${env_record}")"
    ipv4="$(prompt_boolean "Enable IPv4 updates" true "${env_ipv4}")"
    ipv6="$(prompt_boolean "Enable IPv6 updates" false "${env_ipv6}")"
    interval="$(prompt_value "Update interval in seconds" 300 "${env_interval}")"
    create_missing="$(prompt_boolean "Allow creation of a missing record" false "${env_create}")"
    proxied="$(prompt_boolean "Enable Cloudflare proxying" false "${env_proxied}")"

    zone="${zone%.}"
    record="${record%.}"
    validate_dns_name "${zone}" || die "invalid Cloudflare zone: ${zone}"
    validate_dns_name "${record}" || die "invalid DNS record name: ${record}"
    zone_lower="${zone,,}"
    record_lower="${record,,}"
    [[ "${record_lower}" == "${zone_lower}" || "${record_lower}" == *".${zone_lower}" ]] ||
        die "record ${record} does not belong to zone ${zone}"
    [[ "${interval}" =~ ^[0-9]+$ && "${interval}" -ge 60 && "${interval}" -le 86400 ]] ||
        die "the interval must be between 60 and 86400 seconds"
    [[ "${ipv4}" == true || "${ipv6}" == true ]] ||
        die "at least one of IPv4 or IPv6 must be enabled"
    if [[ "${proxied}" == true ]]; then
        warn "Cloudflare proxying is normally unsuitable for SSH, VPN, WireGuard, and other non-HTTP services"
        record_ttl="1"
    fi

    write_token

    temporary_config="$(mktemp "${CONFIG_DIR}/.config.toml.XXXXXX")"
    CONFIG_TEMP="${temporary_config}"
    chmod 0600 "${temporary_config}"
    {
        cat <<EOF
interval_seconds = ${interval}
request_timeout_seconds = 15

[cloudflare]
api_token_file = "${TOKEN_PATH}"

[ipv4]
enabled = ${ipv4}
providers = [
    "https://api.cloudflare.com/cdn-cgi/trace",
    "https://api.ipify.org",
]

[ipv6]
enabled = ${ipv6}
providers = [
    "https://api6.ipify.org",
]
EOF
        if [[ "${ipv4}" == true ]]; then
            cat <<EOF

[[records]]
zone = "${zone}"
name = "${record}"
type = "A"
ttl = ${record_ttl}
proxied = ${proxied}
create_if_missing = ${create_missing}
EOF
        fi
        if [[ "${ipv6}" == true ]]; then
            cat <<EOF

[[records]]
zone = "${zone}"
name = "${record}"
type = "AAAA"
ttl = ${record_ttl}
proxied = ${proxied}
create_if_missing = ${create_missing}
EOF
        fi
    } >"${temporary_config}"

    chown root:"${SERVICE_GROUP}" "${temporary_config}"
    chmod 0640 "${temporary_config}"
    mv -f -- "${temporary_config}" "${CONFIG_PATH}"
    CONFIG_TEMP=""
    log "Wrote ${CONFIG_PATH}; the token remains separate at ${TOKEN_PATH}"
}

validate_configuration() {
    local executable="$1"
    local validation_status=""

    trusted_executable "${executable}" ||
        die "Cirrusync binary is not a trusted root-owned executable: ${executable}"
    [[ -r "${CONFIG_PATH}" ]] || die "configuration is missing: ${CONFIG_PATH}"
    command -v timeout >/dev/null 2>&1 ||
        die "timeout is required for bounded configuration validation"
    log "Validating configuration, token permissions, public IP discovery, and Cloudflare access"
    if run_without_privilege_gain "${SERVICE_USER}" \
        timeout --signal=TERM --kill-after=15s 330s \
            env -i \
            "HOME=${STATE_DIR}" \
            "PATH=/usr/local/bin:/usr/bin:/bin" \
            "RUST_LOG=cirrusync=info" \
            "${executable}" --config "${CONFIG_PATH}" check \
            --allow-edit-probe --allow-create; then
        :
    else
        validation_status="$?"
        terminate_account_processes "${SERVICE_USER}" ||
            warn "configuration validation failed and left processes that could not be terminated"
        die "configuration validation failed or exceeded its deadline (status ${validation_status})"
    fi
    terminate_account_processes "${SERVICE_USER}" ||
        die "configuration validation left processes that could not be terminated"
}

activate_binary() {
    [[ -n "${STAGED_BINARY}" && -x "${STAGED_BINARY}" ]] ||
        die "no staged binary is available"
    mv -f -- "${STAGED_BINARY}" "${BINARY_PATH}"
    STAGED_BINARY=""
    chown root:root "${BINARY_PATH}"
    chmod 0755 "${BINARY_PATH}"
    log "Atomically installed ${BINARY_PATH}"
}

install_systemd_unit() {
    local source_tree="${SOURCE_DIR}"
    local source_unit=""
    local source_unit_sha256=""
    local temporary_unit=""
    local temporary_override=""
    local stage_mode=""
    local build_uid=""

    if [[ -n "${STAGED_SOURCE}" && -d "${STAGED_SOURCE}" ]]; then
        source_tree="${STAGED_SOURCE}"
    fi
    source_unit="${source_tree}/systemd/cirrusync.service"
    assert_directory_chain_is_safe "${source_unit%/*}"
    validate_root_owned_directory "${source_tree}"
    validate_root_owned_directory "${source_unit%/*}"
    validate_root_owned_regular_file "${source_unit}"
    if [[ "${source_tree}" == "${STAGED_SOURCE}" ]]; then
        [[ -d "${SOURCE_STAGE_ROOT}" && ! -L "${SOURCE_STAGE_ROOT}" ]] ||
            die "staged source parent is not a real directory"
        build_uid="$(id -u "${BUILD_USER}")"
        [[ "$(stat -c '%u' "${SOURCE_STAGE_ROOT}")" == "${build_uid}" ]] ||
            die "staged source parent has an unexpected owner"
        stage_mode="$(stat -c '%a' "${SOURCE_STAGE_ROOT}")"
        (( (8#${stage_mode} & 0077) == 0 )) ||
            die "staged source parent permissions are too broad"
    else
        validate_trusted_directory_chain "${source_unit%/*}"
    fi

    temporary_unit="$(mktemp "/etc/systemd/system/.cirrusync.service.XXXXXX")"
    UNIT_TEMP="${temporary_unit}"
    install -o root -g root -m 0644 "${source_unit}" "${temporary_unit}"
    source_unit_sha256="$(sha256sum "${temporary_unit}" | awk '{print $1}')"
    [[ "${source_unit_sha256}" == "${SYSTEMD_UNIT_SHA256}" ]] ||
        die "refusing repository-controlled systemd privileges: trusted unit checksum mismatch"
    mv -f -- "${temporary_unit}" "${UNIT_PATH}"
    UNIT_TEMP=""

    if [[ "${CONFIG_PATH}" == "${DEFAULT_CONFIG_PATH}" ]]; then
        rm -f -- "${UNIT_OVERRIDE_PATH}"
        rmdir --ignore-fail-on-non-empty "${UNIT_OVERRIDE_DIR}" 2>/dev/null || true
    else
        install -d -o root -g root -m 0755 "${UNIT_OVERRIDE_DIR}"
        temporary_override="$(mktemp "${UNIT_OVERRIDE_DIR}/.10-config-path.XXXXXX")"
        OVERRIDE_TEMP="${temporary_override}"
        {
            printf '[Service]\n'
            printf 'ExecStart=\n'
            printf 'ExecStart=%s --config %s run\n' "${BINARY_PATH}" "${CONFIG_PATH}"
            printf 'ReadOnlyPaths=%s\n' "${CONFIG_DIR}"
        } >"${temporary_override}"
        chown root:root "${temporary_override}"
        chmod 0644 "${temporary_override}"
        mv -f -- "${temporary_override}" "${UNIT_OVERRIDE_PATH}"
        OVERRIDE_TEMP=""
    fi

    command -v systemd-analyze >/dev/null 2>&1 ||
        die "systemd-analyze is required to validate the installed unit"
    log "Verifying the systemd unit and drop-in configuration"
    systemd-analyze verify "${UNIT_PATH}"
}

service_remains_healthy() {
    local required_samples="$1"
    local expected_restarts="$2"
    local attempt=""
    local current_restarts=""
    local substate=""

    [[ "${expected_restarts}" =~ ^[0-9]+$ ]] || return 1
    for ((attempt = 0; attempt < required_samples; attempt++)); do
        sleep 1
        systemctl is-active --quiet "${SERVICE_NAME}" || return 1
        substate="$(systemctl show --property=SubState --value \
            "${SERVICE_NAME}")" || return 1
        [[ "${substate}" == running ]] || return 1
        current_restarts="$(systemctl show --property=NRestarts --value \
            "${SERVICE_NAME}")" || return 1
        [[ "${current_restarts}" == "${expected_restarts}" ]] || return 1
        [[ "$(systemctl show --property=ExecMainStatus --value \
            "${SERVICE_NAME}")" == 0 ]] || return 1
    done
}

start_service() {
    local expected_restarts=""

    command -v systemctl >/dev/null 2>&1 || die "systemd is required"
    [[ -d /run/systemd/system ]] ||
        die "systemd is not running; the files are installed but the service cannot be enabled"

    systemctl daemon-reload
    systemctl reset-failed "${SERVICE_NAME}" 2>/dev/null || true
    expected_restarts="$(systemctl show --property=NRestarts --value \
        "${SERVICE_NAME}")" ||
        die "could not read the service restart counter"
    [[ "${expected_restarts}" =~ ^[0-9]+$ ]] ||
        die "systemd returned an invalid service restart counter"
    systemctl enable "${SERVICE_NAME}"
    systemctl restart "${SERVICE_NAME}"
    if service_remains_healthy 5 "${expected_restarts}"; then
        log "${SERVICE_NAME} remained continuously active without restarting"
        return
    fi
    systemctl --no-pager --full status "${SERVICE_NAME}" || true
    die "${SERVICE_NAME} did not remain active; inspect the journal"
}

promote_source() {
    [[ -n "${STAGED_SOURCE}" && -d "${STAGED_SOURCE}" ]] ||
        die "no staged source tree is available"
    [[ "${STAGED_SOURCE}" == "${SOURCE_STAGE_ROOT}/source" ]] ||
        die "internal error: unexpected staged source path"

    SOURCE_HAD_EXISTING=false
    SOURCE_BACKUP="${SOURCE_DIR}.rollback.${BASHPID}"
    [[ ! -e "${SOURCE_BACKUP}" && ! -L "${SOURCE_BACKUP}" ]] ||
        die "stale source rollback path exists: ${SOURCE_BACKUP}"
    if [[ -e "${SOURCE_DIR}" ]]; then
        SOURCE_HAD_EXISTING=true
        mv -- "${SOURCE_DIR}" "${SOURCE_BACKUP}"
    fi
    mv -- "${STAGED_SOURCE}" "${SOURCE_DIR}"
    STAGED_SOURCE="${SOURCE_DIR}"
    log "Promoted the tested source tree to ${SOURCE_DIR}"
}

verify_service_stopped() {
    local load_state=""

    if getent passwd "${SERVICE_USER}" >/dev/null 2>&1; then
        validate_local_system_group "${SERVICE_GROUP}"
        validate_local_system_user \
            "${SERVICE_USER}" "${SERVICE_GROUP}" "${STATE_DIR}"
        SERVICE_ACCOUNT_VALIDATED=true
    fi
    if systemd_is_running; then
        load_state="$(systemctl show --property=LoadState --value \
            "${SERVICE_NAME}" 2>/dev/null)" ||
            die "could not inspect ${SERVICE_NAME} before uninstall"
        [[ -n "${load_state}" ]] ||
            die "systemd returned an empty load state for ${SERVICE_NAME}"
        if [[ "${load_state}" != not-found ]]; then
            quiesce_service ||
                die "refusing to uninstall because ${SERVICE_NAME} could not be quiesced"
        fi
        systemctl disable "${SERVICE_NAME}" >/dev/null 2>&1 || true
        service_enablement_matches false ||
            die "refusing to uninstall because ${SERVICE_NAME} remains enabled"
    fi

    if [[ "${SERVICE_ACCOUNT_VALIDATED}" == true ]]; then
        terminate_account_processes "${SERVICE_USER}" ||
            die "could not terminate processes owned by ${SERVICE_USER}"
    fi
}

remove_system_identity() {
    local user_name="$1"
    local group_name="$2"
    local expected_home="$3"
    local user_uid=""
    local user_processes=""

    if getent passwd "${user_name}" >/dev/null 2>&1; then
        validate_local_system_group "${group_name}"
        validate_local_system_user "${user_name}" "${group_name}" "${expected_home}"
        user_uid="$(id -u "${user_name}")" ||
            die "could not resolve the numeric identity of ${user_name}"
        user_processes="$(list_account_processes "${user_uid}")" ||
            die "could not inspect processes owned by ${user_name}"
        [[ -z "${user_processes}" ]] ||
            die "cannot remove ${user_name} while it owns running processes"
        userdel "${user_name}"
        ! getent passwd "${user_name}" >/dev/null 2>&1 ||
            die "userdel reported success but ${user_name} still resolves"
    fi
    if getent group "${group_name}" >/dev/null 2>&1; then
        validate_local_system_group "${group_name}"
        groupdel "${group_name}"
        ! getent group "${group_name}" >/dev/null 2>&1 ||
            die "groupdel reported success but ${group_name} still resolves"
    fi
}

uninstall() {
    local configuration_deleted=false

    if getent passwd "${BUILD_USER}" >/dev/null 2>&1; then
        validate_local_system_group "${BUILD_GROUP}"
        validate_local_system_user \
            "${BUILD_USER}" "${BUILD_GROUP}" "${BUILD_STATE_DIR}"
        BUILD_ACCOUNT_VALIDATED=true
        terminate_account_processes "${BUILD_USER}" ||
            die "could not terminate processes owned by ${BUILD_USER}"
    fi

    log "Stopping and disabling ${SERVICE_NAME}"
    verify_service_stopped

    rm -f -- "${UNIT_PATH}" "${UNIT_OVERRIDE_PATH}" "${BINARY_PATH}"
    rmdir --ignore-fail-on-non-empty "${UNIT_OVERRIDE_DIR}" 2>/dev/null || true
    if systemd_is_running; then
        systemctl daemon-reload
        systemctl reset-failed "${SERVICE_NAME}" 2>/dev/null || true
    fi

    if confirm "Delete ${CONFIG_PATH} and ${TOKEN_PATH}?"; then
        rm -f -- "${CONFIG_PATH}" "${TOKEN_PATH}"
        rmdir --ignore-fail-on-non-empty "${CONFIG_DIR}" 2>/dev/null || true
        configuration_deleted=true
        log "Deleted the selected configuration and token files"
    else
        log "Configuration and secrets were retained"
    fi

    if confirm "Remove the service account and ${STATE_DIR}?"; then
        [[ "${STATE_DIR}" == "/var/lib/cirrusync" ]] ||
            die "refusing to remove an unexpected state path"
        if [[ "${configuration_deleted}" == true &&
            ! -d "${CONFIG_DIR}" ]]; then
            refuse_mount_point "${STATE_DIR}"
            rm -rf --one-file-system -- "${STATE_DIR}"
            [[ ! -e "${STATE_DIR}" && ! -L "${STATE_DIR}" ]] ||
                die "could not completely remove ${STATE_DIR}; check for nested mounts"
            remove_system_identity \
                "${SERVICE_USER}" "${SERVICE_GROUP}" "${STATE_DIR}"
            log "Removed the service account, group, and state directory"
        else
            warn "Service identity retained because configuration files still depend on its private group"
        fi
    else
        log "Service account and state directory were retained"
    fi

    if confirm "Remove the managed source at ${SOURCE_DIR}?"; then
        [[ "${SOURCE_DIR}" == "/usr/local/src/cirrusync" ]] ||
            die "refusing to remove an unexpected source path"
        refuse_mount_point "${SOURCE_DIR}"
        rm -rf --one-file-system -- "${SOURCE_DIR}"
        [[ ! -e "${SOURCE_DIR}" && ! -L "${SOURCE_DIR}" ]] ||
            die "could not completely remove ${SOURCE_DIR}; check for nested mounts"
        log "Removed the managed source"
    else
        log "Managed source was retained"
    fi

    if confirm "Remove the dedicated build account and managed Rust at ${BUILD_STATE_DIR}?"; then
        [[ "${BUILD_STATE_DIR}" == "/var/lib/cirrusync-build" ]] ||
            die "refusing to remove an unexpected build-state path"
        refuse_mount_point "${BUILD_STATE_DIR}"
        rm -rf --one-file-system -- "${BUILD_STATE_DIR}"
        [[ ! -e "${BUILD_STATE_DIR}" && ! -L "${BUILD_STATE_DIR}" ]] ||
            die "could not completely remove ${BUILD_STATE_DIR}; check for nested mounts"
        remove_system_identity \
            "${BUILD_USER}" "${BUILD_GROUP}" "${BUILD_STATE_DIR}"
        log "Removed the build identity and managed Rust toolchain"
    else
        log "Build identity and managed Rust toolchain were retained"
    fi

    log "Uninstallation complete"
}

print_follow_up() {
    cat <<EOF

Cirrusync is installed and ${SERVICE_NAME} is active.

Useful commands:
  systemctl status ${SERVICE_NAME}
  journalctl -u ${SERVICE_NAME} -f

Full permission check (the service owns the same resource locks):
  sudo systemctl stop ${SERVICE_NAME}
  sudo -u ${SERVICE_USER} -- ${BINARY_PATH} --config ${CONFIG_PATH} check --allow-edit-probe --allow-create
  sudo systemctl start ${SERVICE_NAME}

Update:
  sudo ./bootstrap.sh --repo '${REPO_URL}' --branch '${BRANCH}' --config '${CONFIG_PATH}' --update

Uninstall (configuration and secrets are kept by default):
  sudo ./bootstrap.sh --config '${CONFIG_PATH}' --uninstall
EOF
}

main() {
    parse_arguments "$@"
    ensure_root "$@"
    capture_install_environment
    acquire_installer_lock
    validate_inputs
    choose_action

    if [[ "${ACTION}" == update &&
        ( -n "${TOKEN_INPUT_FILE}" || -n "${INPUT_TOKEN_FILE}" ||
            -n "${INPUT_TOKEN_VALUE}" || -n "${INPUT_ZONE}" ||
            -n "${INPUT_RECORD}" || -n "${INPUT_IPV4}" ||
            -n "${INPUT_IPV6}" || -n "${INPUT_INTERVAL}" ||
            -n "${INPUT_CREATE}" || -n "${INPUT_PROXIED}" ) ]]; then
        die "configuration inputs are not applied during update; use --reconfigure"
    fi

    case "${ACTION}" in
        exit)
            log "No changes made"
            return
            ;;
        uninstall)
            uninstall
            return
            ;;
    esac

    detect_platform
    require_running_systemd

    case "${ACTION}" in
        install | update)
            install_dependencies
            validate_configuration_directory_acl
            create_build_account
            validate_existing_token_before_build
            ensure_rust
            sync_source
            build_project
            begin_transaction
            create_service_account
            secure_existing_configuration_files
            write_configuration
            activate_binary
            install_systemd_unit
            validate_configuration "${BINARY_PATH}"
            start_service
            promote_source
            commit_transaction
            ;;
        reconfigure)
            [[ -x "${BINARY_PATH}" ]] ||
                die "cannot reconfigure because ${BINARY_PATH} is missing"
            [[ -f "${SOURCE_DIR}/systemd/cirrusync.service" ]] ||
                die "managed source is missing; choose update first"
            begin_transaction
            create_service_account
            secure_existing_configuration_files
            write_configuration
            install_systemd_unit
            validate_configuration "${BINARY_PATH}"
            start_service
            commit_transaction
            ;;
        unit)
            [[ -x "${BINARY_PATH}" ]] ||
                die "cannot reinstall the unit because ${BINARY_PATH} is missing"
            [[ -f "${CONFIG_PATH}" ]] ||
                die "cannot reinstall the unit because ${CONFIG_PATH} is missing"
            [[ -f "${SOURCE_DIR}/systemd/cirrusync.service" ]] ||
                die "managed source is missing; choose update first"
            begin_transaction
            create_service_account
            secure_existing_configuration_files
            install_systemd_unit
            validate_configuration "${BINARY_PATH}"
            start_service
            commit_transaction
            ;;
        *)
            die "internal error: unsupported action ${ACTION}"
            ;;
    esac

    print_follow_up
}

if [[ "${BOOTSTRAP_IS_SOURCED}" == false ]]; then
    main "$@"
fi
