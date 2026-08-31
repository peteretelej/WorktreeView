#!/usr/bin/env bash
set -Eeuo pipefail
export LC_ALL=C

readonly termination_grace_seconds=5
readonly preflight_limit_ms=30000

now_ms() {
  local seconds=${EPOCHREALTIME%.*}
  local micros=${EPOCHREALTIME#*.}
  printf '%d\n' "$((10#$seconds * 1000 + 10#$micros / 1000))"
}

preflight_deadline_ms=$(($(now_ms) + preflight_limit_ms))
run_preflight() {
  local remaining_ms timeout_seconds
  remaining_ms=$((preflight_deadline_ms - $(now_ms)))
  timeout_seconds=$(((remaining_ms - termination_grace_seconds * 1000) / 1000))
  (( timeout_seconds > 0 )) || return 124
  timeout --signal=TERM --kill-after="${termination_grace_seconds}s" "${timeout_seconds}s" "$@"
}

uid=$EUID
if ! gid=$(run_preflight id -g); then
  printf 'failed to determine the effective GID within the preflight deadline\n' >&2
  exit 2
fi
if (( uid == 0 || gid == 0 )); then
  printf 'desktop tests must run as a non-root UID and GID\n' >&2
  exit 2
fi

if (( $# != 1 )); then
  printf 'usage: npm run test:desktop -- <absolute-self-contained-fixture-path>\n' >&2
  exit 2
fi

# NUL cannot occur inside a Bash argument.
fixture_input=$1
if [[ "$fixture_input" != /* || "$fixture_input" == *$'\r'* || "$fixture_input" == *$'\n'* || "$fixture_input" == *:* ]]; then
  printf 'fixture must be an absolute path without CR, LF, or colon\n' >&2
  exit 2
fi
if [[ ! -d "$fixture_input" ]]; then
  printf 'fixture must be an existing directory\n' >&2
  exit 2
fi
if ! fixture=$(run_preflight realpath -e -- "$fixture_input"); then
  printf 'failed to canonicalize the fixture within the preflight deadline\n' >&2
  exit 2
fi
if [[ "$fixture" != /* || "$fixture" == *$'\r'* || "$fixture" == *$'\n'* || "$fixture" == *:* ]]; then
  printf 'canonical fixture path contains a rejected delimiter\n' >&2
  exit 2
fi

if ! git_home=$(run_preflight mktemp -d "${TMPDIR:-/tmp}/worktreeview-e2e-home.XXXXXX"); then
  printf 'failed to create the isolated Git home within the preflight deadline\n' >&2
  exit 2
fi
iidfile=
container_id=
container_name=
image_id=
resource_nonce=
docker_resources_possible=0
container_create_started=0
run_dir=
run_dir_owned=0
runner_log=
build_ms=0
runtime_ms=0
cleanup_ms=0

finalize_evidence() {
  local status=0 total_size du_output file size name
  if [[ -n "$iidfile" ]]; then rm -f -- "$iidfile" || status=1; fi
  rm -rf -- "$git_home" || status=1
  if (( run_dir_owned )) && [[ -d "$run_dir" ]]; then
    printf '{"build_ms":%s,"runtime_ms":%s,"cleanup_ms":%s}\n' \
      "$build_ms" "$runtime_ms" "$cleanup_ms" >"$run_dir/timings.json" || status=1
    for name in runner.log backend.log timings.json container-inspect.json worktreeview.png; do
      file="$run_dir/$name"
      if [[ -e "$file" ]]; then
        if size=$(stat -c %s -- "$file"); then
          case "$name" in
            runner.log|backend.log) (( size <= 1048576 )) || status=1 ;;
            timings.json|container-inspect.json) (( size <= 262144 )) || status=1 ;;
            worktreeview.png) (( size > 0 && size <= 5242880 )) || status=1 ;;
          esac
        else
          status=1
        fi
      fi
    done
    for file in "$run_dir"/*; do
      [[ -e "$file" || -L "$file" ]] || continue
      case ${file##*/} in
        runner.log|backend.log|timings.json|container-inspect.json|worktreeview.png) ;;
        *) status=1; rm -rf -- "$file" || status=1 ;;
      esac
    done
    if du_output=$(du -sb -- "$run_dir"); then
      total_size=${du_output%%$'\t'*}
      [[ "$total_size" =~ ^[0-9]+$ ]] && (( total_size <= 8388608 )) || status=1
    else
      status=1
    fi
  fi
  return "$status"
}

run_cleanup_docker() {
  local cleanup_resource_deadline_ms=$1
  shift
  local remaining_ms timeout_seconds
  remaining_ms=$((cleanup_resource_deadline_ms - $(now_ms)))
  timeout_seconds=$(((remaining_ms - termination_grace_seconds * 1000) / 1000))
  (( timeout_seconds > 10 )) && timeout_seconds=10
  (( timeout_seconds > 0 )) || return 124
  timeout --signal=TERM --kill-after="${termination_grace_seconds}s" "${timeout_seconds}s" "$@"
}

remove_owned_container() {
  local cleanup_resource_deadline_ms=$1
  local output candidate inspected inspected_id inspected_run inspected_nonce
  if ! output=$(run_cleanup_docker "$cleanup_resource_deadline_ms" docker container ls --all --quiet --no-trunc \
    --filter "name=^/${container_name}$" \
    --filter "label=com.worktreeview.e2e.run-id=$run_id" \
    --filter "label=com.worktreeview.e2e.resource-nonce=$resource_nonce"); then
    return 1
  fi
  if [[ -z "$output" ]]; then
    container_id=
    container_name=
    return 0
  fi
  [[ "$output" != *$'\n'* && "$output" =~ ^[0-9a-f]{64}$ ]] || return 1
  candidate=$output
  if ! inspected=$(run_cleanup_docker "$cleanup_resource_deadline_ms" docker container inspect --format \
    '{{.Id}}|{{index .Config.Labels "com.worktreeview.e2e.run-id"}}|{{index .Config.Labels "com.worktreeview.e2e.resource-nonce"}}' "$candidate"); then
    return 1
  fi
  IFS='|' read -r inspected_id inspected_run inspected_nonce <<<"$inspected"
  [[ "$inspected_id" == "$candidate" && "$inspected_run" == "$run_id" && "$inspected_nonce" == "$resource_nonce" ]] || return 1
  container_id=$candidate
  run_cleanup_docker "$cleanup_resource_deadline_ms" docker container rm -f -- "$container_id" >/dev/null 2>&1
}

remove_owned_images() {
  local cleanup_resource_deadline_ms=$1
  local output candidate inspected inspected_id inspected_run inspected_nonce status=0
  local -a candidates=()
  if ! output=$(run_cleanup_docker "$cleanup_resource_deadline_ms" docker image ls --all --quiet --no-trunc \
    --filter "label=com.worktreeview.e2e.run-id=$run_id" \
    --filter "label=com.worktreeview.e2e.resource-nonce=$resource_nonce"); then
    return 1
  fi
  if [[ -z "$output" ]]; then
    image_id=
    images_resolved=1
    return 0
  fi
  images_resolved=0
  mapfile -t candidates <<<"$output"
  for candidate in "${candidates[@]}"; do
    [[ "$candidate" =~ ^sha256:[0-9a-f]{64}$ ]] || return 1
    if ! inspected=$(run_cleanup_docker "$cleanup_resource_deadline_ms" docker image inspect --format \
      '{{.Id}}|{{index .Config.Labels "com.worktreeview.e2e.run-id"}}|{{index .Config.Labels "com.worktreeview.e2e.resource-nonce"}}' "$candidate"); then
      status=1
      continue
    fi
    IFS='|' read -r inspected_id inspected_run inspected_nonce <<<"$inspected"
    if [[ "$inspected_id" != "$candidate" || "$inspected_run" != "$run_id" || "$inspected_nonce" != "$resource_nonce" ]]; then
      status=1
      continue
    fi
    image_id=$candidate
    run_cleanup_docker "$cleanup_resource_deadline_ms" docker image rm -- "$candidate" >/dev/null 2>&1 || status=1
  done
  return "$status"
}

cleanup() {
  local original_status=$?
  local cleanup_start cleanup_end cleanup_deadline cleanup_resource_deadline remaining_ms timeout_seconds resources_resolved images_resolved
  trap - EXIT
  set +e
  cleanup_start=$(now_ms)
  cleanup_deadline=$((cleanup_start + 120000))
  cleanup_resource_deadline=$((cleanup_deadline - 30000))
  if (( docker_resources_possible )); then
    resources_resolved=0
    images_resolved=0
    while (( $(now_ms) < cleanup_resource_deadline )); do
      if (( container_create_started )) && [[ -n "$container_name" ]]; then
        remove_owned_container "$cleanup_resource_deadline" || true
      fi
      remove_owned_images "$cleanup_resource_deadline" || true
      if { (( ! container_create_started )) || [[ -z "$container_name" ]]; } && (( images_resolved )); then
        resources_resolved=1
        break
      fi
      sleep 1
    done
    (( resources_resolved )) || original_status=1
  fi
  cleanup_ms=$(($(now_ms) - cleanup_start))
  remaining_ms=$((cleanup_deadline - $(now_ms)))
  timeout_seconds=$(((remaining_ms - 6000) / 1000))
  if (( timeout_seconds > 0 )); then
    export iidfile git_home run_dir run_dir_owned build_ms runtime_ms cleanup_ms
    export -f finalize_evidence
    timeout --signal=TERM --kill-after=5s "${timeout_seconds}s" bash -c finalize_evidence
    if (( $? != 0 )); then original_status=1; fi
  else
    original_status=1
  fi
  cleanup_end=$(now_ms)
  (( cleanup_end <= cleanup_deadline )) || original_status=1
  exit "$original_status"
}
trap cleanup EXIT

git_env=(env HOME="$git_home" GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null)
if ! inside_work_tree=$(run_preflight "${git_env[@]}" git -C "$fixture" rev-parse --is-inside-work-tree 2>/dev/null); then
  printf 'fixture is not a Git work tree\n' >&2
  exit 2
fi
if ! top_level=$(run_preflight "${git_env[@]}" git -C "$fixture" rev-parse --show-toplevel 2>/dev/null); then
  printf 'fixture has no work-tree root\n' >&2
  exit 2
fi
if ! git_dir=$(run_preflight "${git_env[@]}" git -C "$fixture" rev-parse --absolute-git-dir 2>/dev/null); then
  printf 'fixture has no Git metadata\n' >&2
  exit 2
fi
if ! common_dir=$(run_preflight "${git_env[@]}" git -C "$fixture" rev-parse --path-format=absolute --git-common-dir 2>/dev/null); then
  printf 'fixture has no Git common directory\n' >&2
  exit 2
fi
[[ "$inside_work_tree" == "true" ]] || { printf 'bare repositories are not supported\n' >&2; exit 2; }
[[ "$top_level" == "$fixture" ]] || { printf 'fixture must be the repository top level\n' >&2; exit 2; }
case "$git_dir" in
  "$fixture"|"$fixture"/*) ;;
  *) printf 'Git metadata must be inside the fixture\n' >&2; exit 2 ;;
esac
case "$common_dir" in
  "$fixture"|"$fixture"/*) ;;
  *) printf 'Git common metadata must be inside the fixture\n' >&2; exit 2 ;;
esac

if ! run_timestamp=$(run_preflight date -u +%Y%m%dT%H%M%S%N); then
  printf 'failed to create a run timestamp within the preflight deadline\n' >&2
  exit 2
fi
run_id=${WORKTREEVIEW_E2E_RUN_ID:-"run-$run_timestamp-$$-$RANDOM"}
if [[ ! "$run_id" =~ ^[A-Za-z0-9._-]+$ ]]; then
  printf 'WORKTREEVIEW_E2E_RUN_ID must match [A-Za-z0-9._-]+\n' >&2
  exit 2
fi
resource_nonce="$run_timestamp-$$-$RANDOM"
container_name="worktreeview-e2e-$resource_nonce"
force_failure=${WORKTREEVIEW_E2E_FORCE_FAILURE:-}
if [[ -n "$force_failure" && "$force_failure" != 1 ]]; then
  printf 'WORKTREEVIEW_E2E_FORCE_FAILURE may only be 1\n' >&2
  exit 2
fi

artifact_parent=artifacts/tauri-e2e
if [[ -L "$artifact_parent" ]]; then
  printf 'artifact parent may not be a symlink\n' >&2
  exit 2
fi
run_preflight mkdir -p -- "$artifact_parent"
run_dir="$artifact_parent/$run_id"
if [[ -e "$run_dir" || -L "$run_dir" ]]; then
  printf 'artifact run directory already exists\n' >&2
  exit 2
fi
run_preflight mkdir -- "$run_dir"
run_dir_owned=1
run_dir_abs=$(run_preflight realpath -e -- "$run_dir")
runner_log="$run_dir/runner.log"
: >"$runner_log"
: >"$run_dir/backend.log"
printf 'Artifacts: %s\n' "$run_dir_abs"
printf 'Artifacts: %s\n' "$run_dir_abs" >>"$runner_log"

if ! iidfile=$(run_preflight mktemp "${TMPDIR:-/tmp}/worktreeview-e2e-iid.XXXXXX"); then
  printf 'failed to create the image ID file within the preflight deadline\n' >&2
  exit 2
fi
trap cleanup EXIT

run_preflight docker info >/dev/null
run_preflight docker buildx version >/dev/null

# 30s preflight + 30m5s build + 10m5s runtime + 2m cleanup = 42m40s.
docker_resources_possible=1

bounded_stream() {
  local line count bytes overflow=0 tail_file="$runner_log.tail"
  bytes=$(stat -c %s -- "$runner_log") || return
  while IFS= read -r line || [[ -n "$line" ]]; do
    printf '%s\n' "$line" || return
    count=$((${#line} + 1))
    if (( count > 1048576 )); then
      printf '%s\n' "$line" | tail -c 1048576 >"$tail_file" || return
      mv -- "$tail_file" "$runner_log" || return
      bytes=1048576
      overflow=1
    elif (( bytes + count <= 1048576 )); then
      printf '%s\n' "$line" >>"$runner_log" || return
      bytes=$((bytes + count))
    else
      printf '%s\n' "$line" >>"$runner_log" || return
      tail -c 1048576 -- "$runner_log" >"$tail_file" || return
      mv -- "$tail_file" "$runner_log" || return
      bytes=1048576
      overflow=1
    fi
  done
  bytes=$(stat -c %s -- "$runner_log") || return
  if (( bytes > 1048576 )); then
    tail -c 1048576 -- "$runner_log" >"$tail_file" || return
    mv -- "$tail_file" "$runner_log" || return
    overflow=1
  fi
  (( overflow == 0 ))
}

build_start=$(now_ms)
build_cmd=(
  timeout --signal=TERM --kill-after=5s 30m
  env DOCKER_BUILDKIT=1 docker buildx build --load
  --iidfile "$iidfile"
  --label "com.worktreeview.e2e.run-id=$run_id"
  --label "com.worktreeview.e2e.resource-nonce=$resource_nonce"
  -f e2e/Dockerfile .
)
set +e
"${build_cmd[@]}" 2>&1 | bounded_stream
build_status=("${PIPESTATUS[@]}")
set -e
build_ms=$(($(now_ms) - build_start))
mapfile -t iid_lines <"$iidfile"
if (( ${#iid_lines[@]} == 1 )) && [[ ${iid_lines[0]} =~ ^sha256:[0-9a-f]{64}$ ]]; then
  image_id=${iid_lines[0]}
fi
if (( build_status[0] != 0 || build_status[1] != 0 )); then
  printf 'desktop image build failed or exceeded its log/deadline bound\n' >&2
  exit 1
fi

container_deadline_ms=$(($(now_ms) + 600000))
run_with_container_deadline() {
  local remaining_ms remaining_seconds
  remaining_ms=$((container_deadline_ms - $(now_ms)))
  (( remaining_ms > 0 )) || return 124
  remaining_seconds=$((remaining_ms / 1000))
  (( remaining_seconds > 0 )) || return 124
  timeout --signal=TERM --kill-after=5s "${remaining_seconds}s" "$@"
}

if [[ -z "$image_id" ]]; then
  printf 'BuildKit did not produce one immutable image ID\n' >&2
  exit 1
fi
image_inspect=(docker image inspect "$image_id")
run_with_container_deadline "${image_inspect[@]}" >/dev/null
image_label=(docker image inspect --format '{{ index .Config.Labels "com.worktreeview.e2e.run-id" }}' "$image_id")
[[ $(run_with_container_deadline "${image_label[@]}") == "$run_id" ]] || { printf 'image ownership label mismatch\n' >&2; exit 1; }
image_nonce=(docker image inspect --format '{{ index .Config.Labels "com.worktreeview.e2e.resource-nonce" }}' "$image_id")
[[ $(run_with_container_deadline "${image_nonce[@]}") == "$resource_nonce" ]] || { printf 'image ownership nonce mismatch\n' >&2; exit 1; }

create_cmd=(
  docker container create
  --name "$container_name"
  --label "com.worktreeview.e2e.run-id=$run_id"
  --label "com.worktreeview.e2e.resource-nonce=$resource_nonce"
  --network none
  --cap-drop ALL
  --security-opt no-new-privileges:true
  --cpus 2
  --memory 4g
  --pids-limit 512
  --shm-size 1g
  --user "$uid:$gid"
  --volume "$fixture:/fixtures/worktreeview:ro"
  --volume "$run_dir_abs:/artifacts:rw"
  --env "WORKTREEVIEW_E2E_RUN_ID=$run_id"
)
if [[ "$force_failure" == 1 ]]; then
  create_cmd+=(--env WORKTREEVIEW_E2E_FORCE_FAILURE=1)
fi
create_cmd+=("$image_id")
container_create_started=1
created_id=$(run_with_container_deadline "${create_cmd[@]}")
[[ "$created_id" =~ ^[0-9a-f]{64}$ ]] || { printf 'Docker returned an invalid container ID\n' >&2; exit 1; }
container_id=$created_id

inspect_cmd=(docker container inspect "$container_id")
inspect_json=$(run_with_container_deadline "${inspect_cmd[@]}")
if (( ${#inspect_json} > 262144 )); then
  printf 'container inspection exceeded 256 KiB\n' >&2
  exit 1
fi
printf '%s\n' "$inspect_json" >"$run_dir/container-inspect.json"
node - "$run_dir/container-inspect.json" "$fixture" "$run_dir_abs" "$uid" "$gid" "$image_id" "$run_id" "$resource_nonce" <<'NODE'
const fs = require("node:fs");
const [inspectPath, fixture, artifacts, uid, gid, imageId, runId, resourceNonce] = process.argv.slice(2);
const parsed = JSON.parse(fs.readFileSync(inspectPath, "utf8"));
if (!Array.isArray(parsed) || parsed.length !== 1) throw new Error("expected one container inspection");
const item = parsed[0];
const host = item.HostConfig;
if (host.NetworkMode !== "none") throw new Error("network mode");
if (!host.CapDrop?.includes("ALL")) throw new Error("capability drop");
if (!host.SecurityOpt?.includes("no-new-privileges:true")) throw new Error("security option");
if (host.NanoCpus !== 2_000_000_000) throw new Error("CPU limit");
if (host.Memory !== 4 * 1024 ** 3) throw new Error("memory limit");
if (host.PidsLimit !== 512) throw new Error("PID limit");
if (host.ShmSize !== 1024 ** 3) throw new Error("shared-memory limit");
if (item.Config.User !== `${uid}:${gid}` || uid === "0" || gid === "0") throw new Error("effective user");
if (item.Image !== imageId || !/^sha256:[0-9a-f]{64}$/.test(item.Image)) throw new Error("immutable image");
if (item.Config.Labels?.["com.worktreeview.e2e.run-id"] !== runId) throw new Error("container ownership");
if (item.Config.Labels?.["com.worktreeview.e2e.resource-nonce"] !== resourceNonce) throw new Error("container ownership nonce");
if (item.Mounts.length !== 2) throw new Error("unexpected mount count");
const fixtureMount = item.Mounts.find((mount) => mount.Destination === "/fixtures/worktreeview");
const artifactMount = item.Mounts.find((mount) => mount.Destination === "/artifacts");
if (!fixtureMount || fixtureMount.Source !== fixture || fixtureMount.RW !== false) throw new Error("fixture mount");
if (!artifactMount || artifactMount.Source !== artifacts || artifactMount.RW !== true) throw new Error("artifact mount");
for (const mount of item.Mounts) {
  if (/docker\.sock|\.X11-unix|wayland|\.ssh|credentials/i.test(`${mount.Source}\n${mount.Destination}`)) {
    throw new Error("forbidden mount");
  }
}
NODE

rm -f -- "$iidfile"
iidfile=
runtime_start=$(now_ms)
start_cmd=(docker container start --attach "$container_id")
set +e
run_with_container_deadline "${start_cmd[@]}" 2>&1 | bounded_stream
runtime_status=("${PIPESTATUS[@]}")
set -e
runtime_ms=$(($(now_ms) - runtime_start))
if (( runtime_status[0] != 0 || runtime_status[1] != 0 )); then
  printf 'desktop suite failed or exceeded its log/deadline bound\n' >&2
  exit 1
fi
