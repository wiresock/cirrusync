#!/usr/bin/env bash

set -Eeuo pipefail
IFS=$'\n\t'

REPOSITORY_ROOT="$(
    cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." >/dev/null
    pwd -P
)"
readonly REPOSITORY_ROOT
BOOTSTRAP_TEST_ROOT="$(mktemp -d)"
readonly BOOTSTRAP_TEST_ROOT
export CIRRUSYNC_BOOTSTRAP_TEST_ROOT="${BOOTSTRAP_TEST_ROOT}"
trap 'rm -rf -- "${BOOTSTRAP_TEST_ROOT}"' EXIT

# shellcheck source=../bootstrap.sh
source "${REPOSITORY_ROOT}/bootstrap.sh"
unset CIRRUSYNC_BOOTSTRAP_TEST_ROOT

fail() {
    printf 'bootstrap function test failed: %s\n' "$*" >&2
    exit 1
}

bash "${REPOSITORY_ROOT}/bootstrap.sh" --help >/dev/null ||
    fail "direct --help execution failed"
bash -s -- --help <"${REPOSITORY_ROOT}/bootstrap.sh" >/dev/null ||
    fail "piped --help execution failed"

expect_failure() {
    local description="$1"
    shift
    if ("$@") >/dev/null 2>&1; then
        fail "${description}: command unexpectedly succeeded"
    fi
}

test_repository_validation() {
    (
        validate_repository_url \
            "https://github.com/wiresock/cirrusync.git"
    ) || fail "the canonical repository URL was rejected"

    expect_failure "repository credentials" \
        validate_repository_url "https://token@github.com/wiresock/cirrusync.git"
    expect_failure "terminal control character" \
        validate_repository_url $'https://github.com/wiresock/\e[31m.git'
    expect_failure "non-HTTPS repository" \
        validate_repository_url "ssh://git@github.com/wiresock/cirrusync.git"
    expect_failure "repository traversal" \
        validate_repository_url "https://github.com/wiresock/../other.git"
    expect_failure "invalid repository port" \
        validate_repository_url "https://github.com:99999/wiresock/cirrusync.git"
}

test_argument_modes() {
    (
        parse_arguments --non-interactive --reconfigure
        [[ "${NON_INTERACTIVE}" == true &&
            "${RECONFIGURE_REQUESTED}" == true ]]
    ) || fail "--reconfigure parsing did not set the expected state"

    expect_failure "conflicting actions" \
        parse_arguments --update --reconfigure
}

test_snapshot_restore() {
    local workspace=""
    workspace="$(mktemp -d)"
    (
        ROLLBACK_DIR="${workspace}/rollback"
        mkdir "${ROLLBACK_DIR}"
        printf 'before\n' >"${workspace}/managed"
        chmod 0640 "${workspace}/managed"
        snapshot_path "${workspace}/managed" managed
        rm "${workspace}/managed"
        restore_snapshot "${workspace}/managed" managed
        [[ "$(cat "${workspace}/managed")" == before ]]
        [[ "$(stat -c '%a' "${workspace}/managed")" == 640 ]]

        snapshot_path "${workspace}/absent" absent
        printf 'temporary\n' >"${workspace}/absent"
        restore_snapshot "${workspace}/absent" absent
        [[ ! -e "${workspace}/absent" ]]
    ) || {
        rm -rf -- "${workspace}"
        fail "snapshot restoration did not preserve presence, data, and mode"
    }
    rm -rf -- "${workspace}"
}

test_snapshot_restore_failure_is_atomic() {
    local workspace=""
    workspace="$(mktemp -d)"
    (
        ROLLBACK_DIR="${workspace}/rollback"
        mkdir "${ROLLBACK_DIR}"
        printf 'before\n' >"${workspace}/managed"
        snapshot_path "${workspace}/managed" managed
        printf 'after\n' >"${workspace}/managed"
        cp() { return 1; }

        if restore_snapshot "${workspace}/managed" managed; then
            return 1
        fi
        [[ "$(cat "${workspace}/managed")" == after ]]
    ) || {
        rm -rf -- "${workspace}"
        fail "a failed snapshot copy partially replaced the live file"
    }
    rm -rf -- "${workspace}"
}

test_nested_mount_rejection() {
    local workspace=""
    workspace="$(mktemp -d)"
    mkdir "${workspace}/managed"
    (
        findmnt() {
            printf '/\n%s/nested\n' "${workspace}/managed"
        }
        expect_failure "nested mount removal" \
            refuse_mount_point "${workspace}/managed"
    ) || {
        rm -rf -- "${workspace}"
        fail "nested mount detection did not fail closed"
    }
    (
        findmnt() { printf '/\n'; }
        refuse_mount_point "${workspace}/managed"
    ) || {
        rm -rf -- "${workspace}"
        fail "ordinary managed directory was incorrectly treated as a mount"
    }
    rm -rf -- "${workspace}"
}

test_service_health_sampling() {
    local restart_counter=""

    (
        local restarts=2
        sleep() { :; }
        systemctl() {
            case "$1 $2" in
                "show --property=NRestarts")
                    printf '%s\n' "${restarts}"
                    ;;
                "show --property=SubState")
                    printf 'running\n'
                    ;;
                "show --property=ExecMainStatus")
                    printf '0\n'
                    ;;
                "is-active --quiet")
                    return 0
                    ;;
                *)
                    return 1
                    ;;
            esac
        }
        service_remains_healthy 3 2
    ) || fail "stable service was reported unhealthy"

    restart_counter="$(mktemp)"
    printf '0\n' >"${restart_counter}"
    if bash -c '
        set -Eeuo pipefail
        source "$1"
        counter_file="$2"
        sleep() { :; }
        systemctl() {
            local sample
            case "$1 $2" in
                "show --property=NRestarts")
                    sample="$(cat "${counter_file}")"
                    sample=$((sample + 1))
                    printf "%s\n" "${sample}" >"${counter_file}"
                    if ((sample >= 3)); then printf "1\n"; else printf "0\n"; fi
                    ;;
                "show --property=SubState") printf "running\n" ;;
                "show --property=ExecMainStatus") printf "0\n" ;;
                "is-active --quiet") return 0 ;;
                *) return 1 ;;
            esac
        }
        service_remains_healthy 3 0
    ' _ "${REPOSITORY_ROOT}/bootstrap.sh" "${restart_counter}"; then
        rm -f -- "${restart_counter}"
        fail "restart during health window: command unexpectedly succeeded"
    fi
    rm -f -- "${restart_counter}"
}

test_empty_account_process_list_is_successful() {
    local current_processes=""

    current_processes="$(list_account_processes "$(id -u)")" ||
        fail "real procps process enumeration rejected a valid UID"
    [[ "${current_processes}" =~ ^[0-9]+([[:space:]][0-9]+)*$ ]] ||
        fail "real procps process enumeration returned invalid process identifiers"

    (
        pgrep() {
            [[ "$1" == --euid && "$3" == -- && "$4" == '.*' ]] ||
                return 2
            return 1
        }
        [[ -z "$(list_account_processes 12345)" ]]
    ) || fail "an idle account was treated as a process-inspection failure"

    (
        pgrep() { return 2; }
        expect_failure "fatal process enumeration error" \
            list_account_processes 12345
    ) || fail "a fatal process-inspection failure was treated as an idle account"
}

test_account_process_termination_uses_effective_uid_and_pattern() {
    local workspace=""
    workspace="$(mktemp -d)"
    (
        local running_marker="${workspace}/running"
        local term_marker="${workspace}/term"
        local kill_marker="${workspace}/kill"
        : >"${running_marker}"
        getent() {
            [[ "$1" == passwd && "$2" == "${BUILD_USER}" ]]
        }
        id() {
            [[ "$1" == -u && "$2" == "${BUILD_USER}" ]]
            printf '12345\n'
        }
        pgrep() {
            [[ "$1" == --euid && "$2" == 12345 &&
                "$3" == -- && "$4" == '.*' ]] || return 2
            if [[ -e "${running_marker}" ]]; then
                printf '4242\n'
                return 0
            fi
            return 1
        }
        pkill() {
            [[ "$1" == --signal &&
                ( "$2" == TERM || "$2" == KILL ) &&
                "$3" == --euid && "$4" == 12345 &&
                "$5" == -- && "$6" == '.*' ]] || return 2
            if [[ "$2" == TERM ]]; then
                : >"${term_marker}"
            else
                : >"${kill_marker}"
                rm -f -- "${running_marker}"
            fi
        }
        sleep() { :; }

        terminate_account_processes "${BUILD_USER}"
        [[ -e "${term_marker}" && -e "${kill_marker}" &&
            ! -e "${running_marker}" ]]
    ) || {
        rm -rf -- "${workspace}"
        fail "account process cleanup omitted effective-UID selection or the mandatory pattern"
    }
    rm -rf -- "${workspace}"
}

test_existing_token_is_safe_before_build() {
    install -d "${CONFIG_ROOT}"
    # These globals are consumed by the sourced validation function.
    # shellcheck disable=SC2034
    TOKEN_PATH="${CONFIG_ROOT}/token"
    printf 'secret\n' >"${TOKEN_PATH}"

    (
        getfacl() {
            printf 'user::rw-\ngroup::---\nother::---\n'
        }
        chmod 0600 "${TOKEN_PATH}"
        validate_existing_token_before_build
    ) || fail "a root-owner-only token was rejected"

    (
        getfacl() {
            printf 'user::rw-\ngroup::r--\nother::r--\n'
        }
        chmod 0644 "${TOKEN_PATH}"
        expect_failure "world-readable managed token" \
            validate_existing_token_before_build
    ) || fail "a world-readable managed token was accepted before build"

    (
        validate_local_system_group() { :; }
        getent() {
            [[ "$1" == group && "$2" == "${SERVICE_GROUP}" ]] ||
                return 1
            printf '%s:x:1:\n' "${SERVICE_GROUP}"
        }
        getfacl() {
            printf 'user::rw-\ngroup::r--\nother::---\n'
        }
        chmod 0640 "${TOKEN_PATH}"
        expect_failure "build-group-readable managed token" \
            validate_existing_token_before_build
    ) || fail "a group-readable token outside the service group was accepted"

    (
        getfacl() {
            printf 'user::rw-\nuser:12345:r--\ngroup::---\nmask::r--\nother::---\n'
        }
        chmod 0600 "${TOKEN_PATH}"
        expect_failure "managed token with an extended ACL" \
            validate_existing_token_before_build
    ) || fail "a token with an extended read ACL was accepted"
}

test_configuration_directory_acl_is_rejected_before_secret_writes() {
    (
        # These globals are consumed by the sourced hardening function.
        # shellcheck disable=SC2034
        CONFIG_DIR="${CONFIG_ROOT}"
        CONFIG_PATH="${CONFIG_ROOT}/config.toml"
        TOKEN_PATH="${CONFIG_ROOT}/token"
        install -d "${CONFIG_DIR}"
        getfacl() {
            printf '%s\n' \
                'user::rwx' \
                'group::r-x' \
                'other::---' \
                'default:user::rwx' \
                'default:user:12345:r-x' \
                'default:group::r-x' \
                'default:mask::r-x' \
                'default:other::---'
        }

        expect_failure "configuration directory default ACL" \
            secure_existing_configuration_files
    ) || fail "a default ACL that could expose a new token was accepted"

    (
        # These globals are consumed by the sourced hardening function.
        # shellcheck disable=SC2034
        CONFIG_DIR="${CONFIG_ROOT}"
        CONFIG_PATH="${CONFIG_ROOT}/config.toml"
        TOKEN_PATH="${CONFIG_ROOT}/token"
        getfacl() {
            printf 'user::rwx\ngroup::r-x\nother::---\n'
        }
        setfacl() { :; }
        chown() { :; }
        chmod() { :; }

        secure_existing_configuration_files
    ) || fail "a configuration directory with only base ACL entries was rejected"
}

test_setid_executable_is_rejected() {
    local executable="${BUILD_STATE_DIR}/setid-test"

    install -d "${BUILD_STATE_DIR}"
    printf '#!/usr/bin/env bash\nexit 0\n' >"${executable}"
    chmod 4755 "${executable}"
    expect_failure "set-ID trusted executable" trusted_executable "${executable}"
    rm -f -- "${executable}"
}

test_bounded_unprivileged_command_is_executable() {
    (
        runuser() {
            [[ "$1" == --user && "$2" == "${BUILD_USER}" && "$3" == -- ]]
            shift 3
            "$@"
        }

        run_without_privilege_gain "${BUILD_USER}" \
            timeout --signal=TERM --kill-after=1s 2s \
            bash -c 'exit 0'
    ) || fail "the no-new-privileges wrapper could not execute a bounded command"
}

test_service_state_queries_fail_closed() {
    (
        systemctl() {
            [[ "$1" == show ]] && return 1
            return 1
        }
        expect_failure "failed systemd activity query" read_service_activity
    ) || fail "a failed systemd activity query was interpreted as inactive"

    (
        systemctl() {
            case "$1" in
                show) printf 'loaded\n' ;;
                is-active)
                    printf 'unknown\n'
                    return 3
                    ;;
                *) return 1 ;;
            esac
        }
        expect_failure "unknown systemd activity state" read_service_activity
    ) || fail "an unknown systemd activity state was accepted"

    (
        systemctl() { return 1; }
        expect_failure "failed systemd enablement query" read_service_enablement
    ) || fail "a failed systemd enablement query was interpreted as disabled"
}

test_quiesce_uses_account_cleanup_after_stop_failure() {
    local cleanup_marker=""
    cleanup_marker="$(mktemp)"
    rm -f -- "${cleanup_marker}"
    (
        local service_state=active
        # This global is consumed by quiesce_service.
        # shellcheck disable=SC2034
        SERVICE_ACCOUNT_VALIDATED=true
        sleep() { :; }
        systemctl() {
            case "$1" in
                stop) return 1 ;;
                show) printf 'loaded\n' ;;
                is-active) printf '%s\n' "${service_state}" ;;
                *) return 1 ;;
            esac
        }
        terminate_account_processes() {
            [[ "$1" == "${SERVICE_USER}" ]]
            service_state=inactive
            : >"${cleanup_marker}"
        }

        quiesce_service
        [[ "${service_state}" == inactive && -e "${cleanup_marker}" ]]
    ) || {
        rm -f -- "${cleanup_marker}"
        fail "account cleanup was not attempted after systemd failed to stop the service"
    }
    rm -f -- "${cleanup_marker}"
}

test_transaction_rollback() {
    local old_rollback_dir=""

    install -d \
        "${BINARY_PATH%/*}" \
        "${SOURCE_DIR}" \
        "${STATE_DIR}" \
        "${UNIT_PATH%/*}" \
        "${UNIT_OVERRIDE_DIR}" \
        "${CONFIG_ROOT}"
    # These globals are consumed by the sourced transaction functions.
    # shellcheck disable=SC2034
    CONFIG_DIR="${CONFIG_ROOT}"
    CONFIG_PATH="${DEFAULT_CONFIG_PATH}"
    TOKEN_PATH="${CONFIG_ROOT}/token"

    printf 'old binary\n' >"${BINARY_PATH}"
    printf 'old unit\n' >"${UNIT_PATH}"
    printf 'old override\n' >"${UNIT_OVERRIDE_PATH}"
    printf 'old config\n' >"${CONFIG_PATH}"
    printf 'old token\n' >"${TOKEN_PATH}"
    printf 'old source\n' >"${SOURCE_DIR}/marker"

    (
        getent() { return 1; }
        chown() { return 0; }
        systemctl() {
            case "$1" in
                is-active) printf 'inactive\n' ;;
                is-enabled)
                    if [[ "${2:-}" == --quiet ]]; then
                        return 1
                    fi
                    printf 'disabled\n'
                    return 1
                    ;;
                show)
                    [[ "$2" == --property=LoadState ]] && printf 'loaded\n'
                    ;;
                *) return 0 ;;
            esac
        }

        begin_transaction
        old_rollback_dir="${ROLLBACK_DIR}"
        printf 'new binary\n' >"${BINARY_PATH}"
        printf 'new unit\n' >"${UNIT_PATH}"
        printf 'new override\n' >"${UNIT_OVERRIDE_PATH}"
        printf 'new config\n' >"${CONFIG_PATH}"
        printf 'new token\n' >"${TOKEN_PATH}"

        # shellcheck disable=SC2034
        SOURCE_HAD_EXISTING=true
        SOURCE_BACKUP="${SOURCE_DIR}.rollback.${BASHPID}"
        mv -- "${SOURCE_DIR}" "${SOURCE_BACKUP}"
        install -d "${SOURCE_DIR}"
        printf 'new source\n' >"${SOURCE_DIR}/marker"

        rollback_transaction
        [[ "$(cat "${BINARY_PATH}")" == "old binary" ]]
        [[ "$(cat "${UNIT_PATH}")" == "old unit" ]]
        [[ "$(cat "${UNIT_OVERRIDE_PATH}")" == "old override" ]]
        [[ "$(cat "${CONFIG_PATH}")" == "old config" ]]
        [[ "$(cat "${TOKEN_PATH}")" == "old token" ]]
        [[ "$(cat "${SOURCE_DIR}/marker")" == "old source" ]]
        [[ ! -e "${old_rollback_dir}" ]]
    ) || fail "transaction rollback did not restore all managed artifacts"

}

test_failed_rollback_does_not_restart_mixed_state() {
    local restart_marker=""
    restart_marker="$(mktemp)"
    rm -f -- "${restart_marker}"
    (
        local service_state=active
        getent() { return 1; }
        chown() { return 0; }
        systemctl() {
            case "$1" in
                is-active) printf '%s\n' "${service_state}" ;;
                is-enabled)
                    printf 'enabled\n'
                    return 0
                    ;;
                stop) service_state=inactive ;;
                restart)
                    service_state=active
                    : >"${restart_marker}"
                    ;;
                show)
                    case "$2" in
                        --property=LoadState) printf 'loaded\n' ;;
                        --property=NRestarts) printf '0\n' ;;
                        --property=SubState) printf 'running\n' ;;
                        --property=ExecMainStatus) printf '0\n' ;;
                    esac
                    ;;
                *) return 0 ;;
            esac
        }

        printf 'last known good\n' >"${BINARY_PATH}"
        begin_transaction
        printf 'partially installed\n' >"${BINARY_PATH}"
        cp() {
            local argument=""
            for argument in "$@"; do
                [[ "${argument}" == "${ROLLBACK_DIR}/binary" ]] && return 1
            done
            command cp "$@"
        }

        if rollback_transaction; then
            return 1
        fi
        [[ ! -e "${restart_marker}" ]]
        [[ "$(cat "${BINARY_PATH}")" == "partially installed" ]]
        command rm -rf -- "${ROLLBACK_DIR}"
    ) || {
        rm -f -- "${restart_marker}"
        fail "rollback restarted the service after a managed-file restore failure"
    }
    rm -f -- "${restart_marker}"
}

test_ambiguous_rollback_restart_is_quiesced() {
    local cleanup_marker=""
    cleanup_marker="$(mktemp)"
    rm -f -- "${cleanup_marker}"
    (
        local restart_attempted=false
        local service_state=active
        getent() { return 1; }
        chown() { return 0; }
        systemctl() {
            case "$1" in
                is-active) printf '%s\n' "${service_state}" ;;
                is-enabled)
                    printf 'enabled\n'
                    return 0
                    ;;
                stop)
                    service_state=inactive
                    if [[ "${restart_attempted}" == true ]]; then
                        : >"${cleanup_marker}"
                    fi
                    ;;
                restart)
                    restart_attempted=true
                    service_state=active
                    return 1
                    ;;
                show)
                    case "$2" in
                        --property=LoadState) printf 'loaded\n' ;;
                        --property=NRestarts) printf '0\n' ;;
                    esac
                    ;;
                *) return 0 ;;
            esac
        }

        printf 'last known good\n' >"${BINARY_PATH}"
        begin_transaction
        if rollback_transaction; then
            return 1
        fi
        [[ "${service_state}" == inactive ]]
        [[ -e "${cleanup_marker}" ]]
        command rm -rf -- "${ROLLBACK_DIR}"
    ) || {
        rm -f -- "${cleanup_marker}"
        fail "an ambiguous rollback restart was not stopped and cleaned up"
    }
    rm -f -- "${cleanup_marker}"
}

test_failed_rollback_stop_is_reported() {
    (
        local service_state=inactive
        sleep() { :; }
        getent() { return 1; }
        chown() { return 0; }
        systemctl() {
            case "$1" in
                is-active) printf '%s\n' "${service_state}" ;;
                is-enabled)
                    if [[ "${2:-}" == --quiet ]]; then
                        return 1
                    fi
                    printf 'disabled\n'
                    return 1
                    ;;
                show)
                    [[ "$2" == --property=LoadState ]] && printf 'loaded\n'
                    ;;
                stop) return 1 ;;
                *) return 0 ;;
            esac
        }

        printf 'last known good\n' >"${BINARY_PATH}"
        begin_transaction
        printf 'live mixed state\n' >"${BINARY_PATH}"
        service_state=active
        if rollback_transaction; then
            return 1
        fi
        [[ "${service_state}" == active ]]
        [[ "$(cat "${BINARY_PATH}")" == "live mixed state" ]]
        [[ -d "${ROLLBACK_DIR}" ]]
        command rm -rf -- "${ROLLBACK_DIR}"
    ) || fail "rollback did not report that a failed stop left the service active"
}

test_failed_pre_restore_cleanup_preserves_live_files() {
    (
        local cleanup_calls=0
        sleep() { :; }
        getent() { return 1; }
        chown() { return 0; }
        systemctl() {
            case "$1" in
                is-active) printf 'inactive\n' ;;
                is-enabled)
                    printf 'disabled\n'
                    return 1
                    ;;
                show)
                    [[ "$2" == --property=LoadState ]] && printf 'loaded\n'
                    ;;
                *) return 0 ;;
            esac
        }

        printf 'last known good\n' >"${BINARY_PATH}"
        begin_transaction
        printf 'live mixed state\n' >"${BINARY_PATH}"
        # This global is consumed by rollback_transaction.
        # shellcheck disable=SC2034
        SERVICE_ACCOUNT_VALIDATED=true
        terminate_account_processes() {
            ((cleanup_calls += 1))
            ((cleanup_calls == 1))
        }

        if rollback_transaction; then
            return 1
        fi
        [[ "${cleanup_calls}" == 2 ]]
        [[ "$(cat "${BINARY_PATH}")" == "live mixed state" ]]
        [[ -d "${ROLLBACK_DIR}" ]]
        command rm -rf -- "${ROLLBACK_DIR}"
    ) || fail "rollback restored files after its final account-idle check failed"
}

test_failed_rollback_disable_is_reported() {
    (
        local enabled=false
        getent() { return 1; }
        chown() { return 0; }
        systemctl() {
            case "$1" in
                is-active) printf 'inactive\n' ;;
                is-enabled)
                    if [[ "${enabled}" == true ]]; then
                        printf 'enabled\n'
                        return 0
                    fi
                    [[ "${2:-}" == --quiet ]] && return 1
                    printf 'disabled\n'
                    return 1
                    ;;
                show)
                    [[ "$2" == --property=LoadState ]] && printf 'loaded\n'
                    ;;
                disable) return 1 ;;
                *) return 0 ;;
            esac
        }

        begin_transaction
        enabled=true
        if rollback_transaction; then
            return 1
        fi
        [[ "${enabled}" == true ]]
        [[ -d "${ROLLBACK_DIR}" ]]
        command rm -rf -- "${ROLLBACK_DIR}"
    ) || fail "rollback did not report that the service remained enabled"
}

test_repository_history_protection() {
    local workspace=""
    workspace="$(mktemp -d)"
    (
        runuser() {
            [[ "$1" == --user ]]
            shift 2
            [[ "$1" == -- ]]
            shift
            "$@"
        }

        git init --quiet --initial-branch=main "${workspace}/base"
        printf 'base\n' >"${workspace}/base/tracked"
        git -C "${workspace}/base" add tracked
        git -C "${workspace}/base" \
            -c user.name="Cirrusync Test" \
            -c user.email="test@cirrusync.invalid" \
            commit --quiet -m base
        git clone --quiet -- "${workspace}/base" "${workspace}/old"
        git clone --quiet -- "${workspace}/base" "${workspace}/new"
        # This global is consumed indirectly by the sourced verification helper.
        # shellcheck disable=SC2034
        GIT_SAFE_CONFIG=/dev/null

        verify_existing_repository_is_recoverable \
            "${workspace}/old" "${workspace}/new" "${workspace}"

        printf 'local commit\n' >>"${workspace}/old/tracked"
        git -C "${workspace}/old" add tracked
        git -C "${workspace}/old" \
            -c user.name="Cirrusync Test" \
            -c user.email="test@cirrusync.invalid" \
            commit --quiet -m local
        expect_failure "local commit preservation" \
            verify_existing_repository_is_recoverable \
            "${workspace}/old" "${workspace}/new" "${workspace}"

        rm -rf -- "${workspace}/old"
        git clone --quiet -- "${workspace}/base" "${workspace}/old"
        git -C "${workspace}/old" branch local-only
        expect_failure "local branch preservation" \
            verify_existing_repository_is_recoverable \
            "${workspace}/old" "${workspace}/new" "${workspace}"

        rm -rf -- "${workspace}/old"
        git clone --quiet -- "${workspace}/base" "${workspace}/old"
        git -C "${workspace}/old" tag local-only
        expect_failure "local tag preservation" \
            verify_existing_repository_is_recoverable \
            "${workspace}/old" "${workspace}/new" "${workspace}"

        rm -rf -- "${workspace}/old"
        git clone --quiet -- "${workspace}/base" "${workspace}/old"
        printf 'stashed\n' >>"${workspace}/old/tracked"
        git -C "${workspace}/old" \
            -c user.name="Cirrusync Test" \
            -c user.email="test@cirrusync.invalid" \
            stash push --quiet
        expect_failure "stash preservation" \
            verify_existing_repository_is_recoverable \
            "${workspace}/old" "${workspace}/new" "${workspace}"
    ) || {
        rm -rf -- "${workspace}"
        fail "repository history protection did not reject every local-only object"
    }
    rm -rf -- "${workspace}"
}

test_repository_validation
test_argument_modes
test_snapshot_restore
test_snapshot_restore_failure_is_atomic
test_nested_mount_rejection
test_service_health_sampling
test_empty_account_process_list_is_successful
test_account_process_termination_uses_effective_uid_and_pattern
test_existing_token_is_safe_before_build
test_configuration_directory_acl_is_rejected_before_secret_writes
test_setid_executable_is_rejected
test_bounded_unprivileged_command_is_executable
test_service_state_queries_fail_closed
test_quiesce_uses_account_cleanup_after_stop_failure
test_transaction_rollback
test_failed_rollback_does_not_restart_mixed_state
test_ambiguous_rollback_restart_is_quiesced
test_failed_rollback_stop_is_reported
test_failed_pre_restore_cleanup_preserves_live_files
test_failed_rollback_disable_is_reported
test_repository_history_protection

printf 'bootstrap function checks passed\n'
