#!/bin/bash

set -Eeuo pipefail

function _usage() {
  echo "holochain deptool - changing Cargo deps for testing"
  echo "usage: deptool [options] cmd"
  echo "commands:"
  echo "  lib3h - deptool lib3h <subcmd>"
  echo "    version - deptool lib3h version <version>"
  echo "    branch - deptool lib3h branch <branch-name>"
  echo "options:"
  echo "  -h --help: additional help for command"
  exit 1
}

function _lib3h_deps() {
  local __dep_str="${1}"
  echo "setting lib3h deps to ${__dep_str}"

  local __deps=$(find ../.. -maxdepth 2 -mindepth 2 -name Cargo.toml; find ../../app_spec/zomes -maxdepth 3 -mindepth 3 -name Cargo.toml)
  echo "${__deps}"
  sed -i'' "s/\\(lib3h[^[:space:]]*[[:space:]]\\+=[[:space:]]\\+\\).*/\\1${__dep_str//\//\\\/}/" ${__deps}
}

function _cmd() {
  local __cmd="${1:-<unset>}"
  case "${__cmd}" in
    lib3h)
      local __sub="${2:-<unset>}"
      case "${__sub}" in
        version)
          if [ ${__help} == 1 ]; then
            echo "deptool lib3h version"
            echo " - set the various lib3h dep versions"
            echo " - example: deptool lib3h version 0.0.9"
            echo "   will set: lib3h = \"=0.0.9\""
            exit 1
          fi
          _lib3h_deps "\"=${3}\""
          ;;
        branch)
          if [ ${__help} == 1 ]; then
            echo "deptool lib3h branch"
            echo " - set the various lib3h dep to a github branch"
            echo " - example: deptool lib3h branch test-a"
            echo "   will set: lib3h = { git = \"https://github.com/holochain/lib3h\", branch = \"test-a\" }"
            exit 1
          fi
          _lib3h_deps "{ git = \"https://github.com/holochain/lib3h\", branch = \"${3}\" }"
          ;;
        *)
          if [ ${__help} == 1 ]; then
            echo "deptool lib3h"
            echo " - alter lib3h dependencies in this repo"
            echo " - example: deptool lib3h version 0.0.9"
            echo " - example: deptool lib3h branch test-a"
            exit 1
          fi
          echo "unexpected lib3h subcommand '${__sub}'"
          _usage
          ;;
      esac
      ;;
    *)
      echo "unexpected command '${__cmd}'"
      _usage
      ;;
  esac
}

function _this_dir() {
  local __src_dir="${BASH_SOURCE[0]}"
  local __work_dir=""
  while [ -h "${__src_dir}" ]; do
    __work_dir="$(cd -P "$(dirname "${__src_dir}")" >/dev/null 2>&1 && pwd)"
    __src_dir="$(readlink "${__src_dir}")"
    [[ ${__src_dir} != /* ]] && __src_dir="${__work_dir}/${__src_dir}"
  done
  __work_dir="$(cd -P "$(dirname "${__src_dir}")" >/dev/null 2>&1 && pwd)"

  cd "${__work_dir}"
}

function main() {
  _this_dir

  local __cmd=""
  local __help="0"
  while (( "${#}" )); do
    case "${1}" in
      -h|--help)
        __help="1"
        shift
        ;;
      --) # end argument parsing
        shift
        break
        ;;
      -*|--*=) # unsupported flags
        echo "Error: Unsupported option ${1}" >&2
        exit 1
        ;;
      *) # preserve positional arguments
        __cmd="$__cmd ${1}"
        shift
        ;;
    esac
  done

  _cmd ${__cmd}
}

main "${@}"
