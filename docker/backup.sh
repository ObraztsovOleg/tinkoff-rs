#!/bin/sh
# Nightly PostgreSQL copies of the stand (runs in the backup service).
# Copies land in the campaign volume: /data/backups/<db>-YYYYMMDD.dump, the newest
# BACKUP_KEEP stay. A missing or day-old copy is made at start. While the
# supervisor reports critically low disk space (/data/DISK_LOW) copies are skipped.
set -eu
DIR=/data/backups
KEEP=${BACKUP_KEEP:-3}
HOUR_UTC=${BACKUP_HOUR_UTC:-1}
mkdir -p "$DIR"
chmod 700 "$DIR"
PGPASSWORD="$(cat /run/secrets/postgres_password)"
export PGPASSWORD

status() {
    printf '{"at":"%s","ok":%s,"message":"%s"}\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$1" "$2" > "$DIR/status.json.tmp"
    mv "$DIR/status.json.tmp" "$DIR/status.json"
}

backup() {
    if [ -e /data/DISK_LOW ]; then
        status false "skipped: disk space is critically low"
        return
    fi
    name="$PGDATABASE-$(date -u +%Y%m%d).dump"
    # Vectors are recomputed from the documents; they are not worth the space.
    if pg_dump -Fc -Z 6 --exclude-table-data=document_embeddings -f "$DIR/$name.partial"; then
        mv "$DIR/$name.partial" "$DIR/$name"
        chmod 600 "$DIR/$name"
        ls -1t "$DIR"/*.dump | tail -n +"$((KEEP + 1))" | xargs -r rm -f
        status true "$name $(wc -c < "$DIR/$name") bytes"
    else
        rm -f "$DIR/$name.partial"
        status false "pg_dump failed"
    fi
}

latest=$(ls -1t "$DIR"/*.dump 2>/dev/null | head -n 1 || true)
if [ -z "$latest" ] || [ -n "$(find "$latest" -mmin +1440)" ]; then
    backup
fi
while :; do
    now=$(date -u +%s)
    next=$(date -u -d "$(date -u +%F) $HOUR_UTC:00:00" +%s)
    if [ "$next" -le "$now" ]; then
        next=$((next + 86400))
    fi
    sleep $((next - now))
    backup
done
