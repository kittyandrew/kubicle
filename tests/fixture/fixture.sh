#!/usr/bin/env bash
# The development mail server: Mailu's own Dovecot image (pinned below by digest) with the fixture mailbox.
#
#   tests/fixture/fixture.sh start   # first start seeds about 58,000 messages and indexes them (a minute or so)
#   tests/fixture/fixture.sh stop    # keeps the mailbox; `start` again resumes it
#   tests/fixture/fixture.sh reset   # deletes the mailbox and the TLS files, so the next start seeds anew
#
# IMAP with STARTTLS listens on 127.0.0.1:$KUBICLE_FIXTURE_PORT (default 10143), the port kubicle uses on Mailu
# front. Sign in as andrew@example.test with password fixture-password (users). The certificate is for
# imap.example.test, signed by a CA the container makes on first start; `start` copies it to $CA_FILE.
# Settings for kubicle: KUBICLE_MAIL_HOST=127.0.0.1 KUBICLE_IMAP_PORT=10143
# KUBICLE_MAIL_TLS_NAME=imap.example.test KUBICLE_MAIL_EXTRA_CA=$CA_FILE.
set -euo pipefail

# The dovecot image of the Mailu release the owner runs, so the server and its plugins match production.
IMAGE=ghcr.io/mailu/dovecot@sha256:39982c825630f6f93d120fa6164ed968eca96abe8300270ed37cbb28d969b909
NAME=kubicle-fixture
VOLUME=kubicle-fixture
PORT=${KUBICLE_FIXTURE_PORT:-10143}
CA_FILE=${KUBICLE_FIXTURE_CA:-${TMPDIR:-/tmp}/kubicle-fixture-ca.pem}
DIR=$(cd "$(dirname "$0")" && pwd)

case ${1:-} in
  start)
    docker run -d --name "$NAME" --memory=4g -p "127.0.0.1:$PORT:143" -v "$VOLUME:/state" -v "$DIR:/fixture:ro" \
      --entrypoint /bin/sh "$IMAGE" /fixture/entrypoint.sh >/dev/null
    until docker exec "$NAME" doveadm -c /fixture/dovecot.conf mailbox list -u andrew@example.test >/dev/null 2>&1; do
      if [ "$(docker inspect -f '{{.State.Running}}' "$NAME")" != true ]; then docker logs "$NAME"; exit 1; fi
      sleep 1
    done
    docker cp -q "$NAME:/state/tls/ca.pem" "$CA_FILE"
    # Builds Dovecot's caches and the full-text index now, so the first sync and body search are not cold.
    docker exec "$NAME" doveadm -c /fixture/dovecot.conf index -u andrew@example.test '*'
    echo "IMAP on 127.0.0.1:$PORT, CA in $CA_FILE"
    ;;
  stop) docker rm -f "$NAME" >/dev/null ;;
  reset)
    docker rm -f "$NAME" >/dev/null 2>&1 || true
    docker volume rm "$VOLUME" >/dev/null
    ;;
  *)
    echo "usage: $0 start|stop|reset" >&2
    exit 2
    ;;
esac
