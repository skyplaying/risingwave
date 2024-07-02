#!/usr/bin/env bash
set -eo pipefail
[ -n "${BACKUP_TEST_MCLI}" ]
[ -n "${BACKUP_TEST_MCLI_CONFIG}" ]
[ -n "${BACKUP_TEST_RW_ALL_IN_ONE}" ]

function stop_cluster() {
  cargo make --allow-private k 1>/dev/null 2>&1 || true
  cargo make --allow-private wait-processes-exit 1>/dev/null 2>&1 || true
}

function clean_all_data {
  cargo make --allow-private clean-data 1>/dev/null 2>&1
}

function get_meta_store_type() {
  meta_store_type=${META_STORE_TYPE:-etcd}
  if [ "${meta_store_type}" = "sql" ]
  then
    if ! command -v sqlite3 &> /dev/null;
    then
        echo "SQLite3 is not installed."
        exit 1
    fi
  fi
  echo "${meta_store_type}"
}

echo "meta store: $(get_meta_store_type)"

function clean_meta_store() {
  meta_store_type=$(get_meta_store_type)
  if [ "$(get_meta_store_type)" = "sql" ]; then
    clean_sqlite_data
  else
    clean_etcd_data
  fi
}

function clean_sqlite_data() {
  tables=$(sqlite3 "${RW_SQLITE_DB}" "select name from sqlite_master where type='table';")
  while IFS= read table
  do
    if [ -z "${table}" ]; then
      break
    fi
    sqlite3 "${RW_SQLITE_DB}" "delete from [${table}]"
  done <<< "${tables}"
}

function clean_etcd_data() {
  cargo make --allow-private clean-etcd-data 1>/dev/null 2>&1
}

function start_cluster() {
  stop_cluster
  if [ "$(get_meta_store_type)" = "sql" ]; then
    cargo make d ci-meta-backup-test-sql 1>/dev/null 2>&1
  else
    cargo make d ci-meta-backup-test-etcd 1>/dev/null 2>&1
  fi
  sleep 5
}

function full_gc_sst() {
  ${BACKUP_TEST_RW_ALL_IN_ONE} risectl hummock trigger-full-gc -s 0 1>/dev/null 2>&1
  # TODO #6482: wait full gc finish deterministically.
  # Currently have to wait long enough.
  sleep 30
}

function manual_compaction() {
  ${BACKUP_TEST_RW_ALL_IN_ONE} risectl hummock trigger-manual-compaction "$@" 1>/dev/null 2>&1
}

function start_meta_store_minio() {
    if [ "$(get_meta_store_type)" = "sql" ]; then
      start_sql_minio
    else
      start_etcd_minio
    fi
}

function start_sql_minio() {
  cargo make d ci-meta-backup-test-restore-sql 1>/dev/null 2>&1
}

function start_etcd_minio() {
  cargo make d ci-meta-backup-test-restore-etcd 1>/dev/null 2>&1
}

function create_mvs() {
  cargo make slt -p 4566 -d dev "e2e_test/backup_restore/tpch_snapshot_create.slt"
}

function query_mvs() {
  cargo make slt -p 4566 -d dev "e2e_test/backup_restore/tpch_snapshot_query.slt"
}

function drop_mvs() {
  cargo make slt -p 4566 -d dev "e2e_test/backup_restore/tpch_snapshot_drop.slt"
}

function backup() {
  local job_id
  job_id=$(${BACKUP_TEST_RW_ALL_IN_ONE} risectl meta backup-meta 2>&1 | grep "backup job succeeded" | awk -F ',' '{print $(NF-1)}'| awk '{print $(NF)}')
  [ -n "${job_id}" ]
  echo "${job_id}"
}

function delete_snapshot() {
  local snapshot_id
  snapshot_id=$1
  ${BACKUP_TEST_RW_ALL_IN_ONE} risectl meta delete-meta-snapshots "${snapshot_id}"
}

function restore() {
  local job_id
  job_id=$1
  meta_store_type=$(get_meta_store_type)
  echo "try to restore snapshot ${job_id}"
  stop_cluster
  clean_meta_store
  start_meta_store_minio
  ${BACKUP_TEST_RW_ALL_IN_ONE} \
  risectl \
  meta \
  restore-meta \
  --meta-store-type "${meta_store_type}" \
  --meta-snapshot-id "${job_id}" \
  --etcd-endpoints 127.0.0.1:2388 \
  --sql-endpoint "sqlite://${RW_SQLITE_DB}?mode=rwc" \
  --backup-storage-url minio://hummockadmin:hummockadmin@127.0.0.1:9301/hummock001 \
  --hummock-storage-url minio://hummockadmin:hummockadmin@127.0.0.1:9301/hummock001 \
  1>/dev/null 2>&1
}

function execute_sql() {
  local sql
  sql=$1
  echo "${sql}" | psql -h localhost -p 4566 -d dev -U root 2>&1
}

function execute_sql_and_expect() {
  local sql
  sql=$1
  local expected
  expected=$2

  echo "execute SQL ${sql}"
  echo "expected string in result: ${expected}"
  query_result=$(execute_sql "${sql}")
  printf "actual result:\n%s\n" "${query_result}"
  result=$(echo "${query_result}" | grep "${expected}")
  [ -n "${result}" ]
}

function get_max_committed_epoch() {
  mce=$(${BACKUP_TEST_RW_ALL_IN_ONE} risectl hummock list-version --verbose 2>&1 | grep committed_epoch | sed -n 's/^.*committed_epoch: \(.*\),/\1/p')
  # always take the smallest one
  echo "${mce}"|sort -n |head -n 1
}

function get_safe_epoch() {
  safe_epoch=$(${BACKUP_TEST_RW_ALL_IN_ONE} risectl hummock list-version --verbose 2>&1 | grep safe_epoch | sed -n 's/^.*safe_epoch: \(.*\),/\1/p')
  # always take the largest one
  echo "${safe_epoch}"|sort -n -r |head -n 1
}

function get_total_sst_count() {
  ${BACKUP_TEST_MCLI} -C "${BACKUP_TEST_MCLI_CONFIG}" \
  find "hummock-minio/hummock001" -name "*.data" |wc -l
}

function get_max_committed_epoch_in_backup() {
  sed_str="s/.*\"state_table_info\":{\"[[:digit:]]*\":{\"committedEpoch\":\"\([[:digit:]]*\)\",\"safeEpoch\":\"\([[:digit:]]*\)\".*/\1/p"
  ${BACKUP_TEST_MCLI} -C "${BACKUP_TEST_MCLI_CONFIG}" \
  cat "hummock-minio/hummock001/backup/manifest.json" | sed -n "${sed_str}"
}

function get_safe_epoch_in_backup() {
  sed_str="s/.*\"state_table_info\":{\"[[:digit:]]*\":{\"committedEpoch\":\"\([[:digit:]]*\)\",\"safeEpoch\":\"\([[:digit:]]*\)\".*/\2/p"
  ${BACKUP_TEST_MCLI} -C "${BACKUP_TEST_MCLI_CONFIG}" \
  cat "hummock-minio/hummock001/backup/manifest.json" | sed -n "${sed_str}"
}

function get_min_pinned_snapshot() {
  s=$(${BACKUP_TEST_RW_ALL_IN_ONE} risectl hummock list-pinned-snapshots 2>&1 | grep "min_pinned_snapshot" | sed -n 's/.*min_pinned_snapshot \(.*\)/\1/p' | sort -n | head -1)
  echo "${s}"
}
