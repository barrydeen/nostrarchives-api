#!/bin/bash

# Daily cleanup script for nostr-api
# Add to crontab: 0 2 * * * /path/to/daily_cleanup.sh

set -euo pipefail

# Configuration - adjust as needed
DB_HOST="46.225.169.182"
DB_USER="nostr_api"
DB_NAME="nostr_api"
SCRIPT_DIR="$(dirname "$0")"
LOG_FILE="/var/log/nostr-api-cleanup.log"

# Ensure log directory exists
mkdir -p "$(dirname "$LOG_FILE")"

# Function to log with timestamp
log() {
    echo "[$(date +'%Y-%m-%d %H:%M:%S')] $*" | tee -a "$LOG_FILE"
}

log "Starting daily cleanup..."

# Run the SQL cleanup script
if psql "postgresql://$DB_USER@$DB_HOST:5432/$DB_NAME" -f "$SCRIPT_DIR/daily_cleanup.sql" >> "$LOG_FILE" 2>&1; then
    log "Daily cleanup completed successfully"
else
    log "ERROR: Daily cleanup failed with exit code $?"
    exit 1
fi

# Optional: Run ANALYZE to update statistics after cleanup
if psql "postgresql://$DB_USER@$DB_HOST:5432/$DB_NAME" -c "ANALYZE;" >> "$LOG_FILE" 2>&1; then
    log "Database statistics updated"
else
    log "WARNING: Failed to update database statistics"
fi

log "Daily cleanup finished"