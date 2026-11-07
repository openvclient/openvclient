#!/usr/bin/env bash
# SSH test lab: every login method, key type and format, a jump host, a
# private Redis to tunnel to, an SFTP-only account and an old-algorithm server.
#
#   scripts/ssh-lab.sh up      build and start the lab (keys in target/ssh-lab-keys)
#   scripts/ssh-lab.sh test    run the lab tests (cargo test … ssh_lab)
#   scripts/ssh-lab.sh down    stop and remove the containers and network
#
# Ports on 127.0.0.1: 52230 main · 52231 PAM keyboard-interactive only ·
# 52232 key AND password · 52233 password + one-time code · 52234 legacy.
# Users: pwuser/pwpass, keyuser (keys; keypw), certuser (certificate),
# otpuser/otppw (+ backup codes), pamuser/pampw, sftponly/sftppw.
# Behind the main server: inner:22 (inner/innerpw, same keys) and labredis:6379.
set -euo pipefail
cd "$(dirname "$0")/.."
KEYS="$PWD/target/ssh-lab-keys"
SRC="$PWD/tests/ssh-lab"

keys() {
  rm -rf "$KEYS" && mkdir -p "$KEYS" && cd "$KEYS"
  g() { ssh-keygen -q "$@"; }
  g -t ed25519 -N '' -C ed25519 -f ed25519
  for b in 256 384 521; do g -t ecdsa -b $b -N '' -C ecdsa$b -f ecdsa$b; done
  for b in 2048 4096; do g -t rsa -b $b -N '' -C rsa$b -f rsa$b; done
  g -t ed25519 -N keypass -C ed25519_enc -f ed25519_enc
  g -t rsa -b 2048 -N keypass -C rsa_enc -f rsa_enc
  g -t rsa -b 2048 -m PEM -N '' -C rsa_pem -f rsa_pem
  g -t rsa -b 2048 -m PEM -N keypass -C rsa_pem_enc -f rsa_pem_enc
  g -t ecdsa -b 256 -m PEM -N '' -C ecdsa_pem -f ecdsa_pem
  g -t rsa -b 2048 -m PKCS8 -N '' -C rsa_pkcs8 -f rsa_pkcs8
  g -t ed25519 -N '' -C ca -f user_ca
  g -t ed25519 -N '' -C certkey -f certkey
  ssh-keygen -q -s user_ca -I certuser -n certuser -V +52w certkey.pub
  g -t ed25519 -N '' -C jumpkey -f jumpkey
  cat ed25519.pub ecdsa*.pub rsa2048.pub rsa4096.pub ed25519_enc.pub rsa_enc.pub rsa_pem.pub rsa_pem_enc.pub ecdsa_pem.pub rsa_pkcs8.pub jumpkey.pub > authorized_keys
  cd - >/dev/null
}

up() {
  [ -f "$KEYS/authorized_keys" ] || keys
  ctx=$(mktemp -d)
  cp -R "$SRC/." "$ctx/" && mkdir -p "$ctx/keys" && cp "$KEYS/authorized_keys" "$KEYS/user_ca.pub" "$ctx/keys/" && cp "$KEYS/authorized_keys" "$ctx/inner/"
  docker build -q -t ovc-ssh-lab "$ctx" >/dev/null
  docker build -q -t ovc-ssh-inner "$ctx/inner" >/dev/null
  rm -rf "$ctx"
  down >/dev/null 2>&1 || true
  docker network create ovclab >/dev/null
  docker run -d --rm --name ovc-lab-inner --network ovclab --network-alias inner ovc-ssh-inner >/dev/null
  docker run -d --rm --name ovc-lab-redis --network ovclab --network-alias labredis redis:7-alpine >/dev/null
  docker run -d --rm --name ovc-lab-main --network ovclab -p 52230:22 -p 52231:2223 -p 52232:2224 -p 52233:2225 -p 52234:2226 ovc-ssh-lab >/dev/null
  echo "SSH lab is up. Keys: $KEYS"
}

down() {
  docker rm -f ovc-lab-main ovc-lab-inner ovc-lab-redis >/dev/null 2>&1 || true
  docker network rm ovclab >/dev/null 2>&1 || true
}

case "${1:-}" in
  up) up ;;
  down) down; echo "SSH lab removed." ;;
  test) OPENVCLIENT_SSH_LAB_KEYS="$KEYS" cargo test -p openvclient-storage ssh_lab -- --nocapture ;;
  keys) keys ;;
  *) echo "usage: $0 up|test|down|keys" >&2; exit 2 ;;
esac
