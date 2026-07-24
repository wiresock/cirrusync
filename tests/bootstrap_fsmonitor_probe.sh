#!/usr/bin/env bash

set -Eeuo pipefail

# Git runs this repository-local command while refreshing the managed checkout.
# The lifecycle test asserts that it runs unprivileged and that the installer
# terminates a process it leaves behind before creating the next staging tree.
readonly PID_FILE="/var/tmp/cirrusync-fsmonitor-probe.pid"

existing_pid=""
if [[ -r "${PID_FILE}" ]]; then
    existing_pid="$(<"${PID_FILE}")"
fi
if [[ ! "${existing_pid}" =~ ^[0-9]+$ ]] ||
    ! kill -0 "${existing_pid}" 2>/dev/null; then
    nohup bash -c '
        for ((attempt = 0; attempt < 6000; attempt++)); do
            for git_directory in /usr/local/src/.cirrusync-source.*/source/.git; do
                if [[ -d "${git_directory}" ]]; then
                    : > /var/tmp/cirrusync-fsmonitor-contaminated
                    : > "${git_directory}/cirrusync-fsmonitor-contaminated"
                    exit
                fi
            done
            sleep 0.05
        done
    ' </dev/null >/dev/null 2>&1 &
    printf '%s\n' "$!" >"${PID_FILE}"
fi

id -u >>/var/tmp/cirrusync-fsmonitor-probe.uid
printf '/\0'
