#!/usr/bin/env bash
# Pull the gateway from git and rebuild it, but only swap containers AFTER a
# successful build so live connections are never dropped for a build that fails.
# Run by systemd on a timer; a flock keeps two runs from overlapping.
#
# What counts as "deployed" is the sha in .deployed, written only once the new
# container is up. Comparing against the checkout instead meant a failed build
# left the tree on the new commit and every later tick saw nothing to do — the
# gateway then sat on the last good image until somebody pushed again.
set -euo pipefail

REPO=/opt/velvetdesk
BRANCH=master
LOG="$REPO/deploy.log"
STAMP="$REPO/.deployed"

exec 9>"$REPO/.deploy.lock"
flock -n 9 || { echo "$(date -Is) another deploy is running, skipping" >>"$LOG"; exit 0; }

cd "$REPO"

log() { echo "$(date -Is) $*" >>"$LOG"; }

git fetch --quiet origin "$BRANCH"
REMOTE=$(git rev-parse "origin/$BRANCH")
DEPLOYED=$(cat "$STAMP" 2>/dev/null || git rev-parse HEAD 2>/dev/null || echo none)

if [ "$DEPLOYED" = "$REMOTE" ]; then
  exit 0
fi

log "new commit $REMOTE (deployed $DEPLOYED) — deploying"
git reset --hard "origin/$BRANCH" >>"$LOG" 2>&1

# Build the new image while the OLD container keeps serving. This is the long
# step and it causes no downtime.
if ! docker compose build gateway >>"$LOG" 2>&1; then
  log "build FAILED for $REMOTE — keeping the running container, retrying next tick"
  exit 1
fi

# Build is good: swap to it now. compose recreates only the one service; the
# gap is a few seconds, and it only happens once the new image is ready.
if ! docker compose up -d gateway >>"$LOG" 2>&1; then
  log "start FAILED for $REMOTE — retrying next tick"
  exit 1
fi

echo "$REMOTE" >"$STAMP"
log "deployed $REMOTE"

docker image prune -f >>"$LOG" 2>&1 || true
