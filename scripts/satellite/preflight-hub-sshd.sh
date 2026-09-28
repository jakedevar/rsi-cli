#!/bin/sh
# Run through an authenticated SSH command on the hub before enabling dial-home.
set -eu

if [ -z "${SSH_CONNECTION:-}" ]; then
  echo 'cannot inspect hub sshd: SSH_CONNECTION is unavailable' >&2
  exit 1
fi
read -r client_addr client_port server_addr server_port <<EOF
$SSH_CONNECTION
EOF
if [ -z "$client_addr" ] || [ -z "$client_port" ] ||
   [ -z "$server_addr" ] || [ -z "$server_port" ]; then
  echo 'cannot inspect hub sshd: incomplete SSH_CONNECTION' >&2
  exit 1
fi

if command -v sshd >/dev/null 2>&1; then
  sshd_bin=sshd
elif [ -x /usr/sbin/sshd ]; then
  sshd_bin=/usr/sbin/sshd
else
  echo 'cannot inspect hub sshd: sshd executable unavailable' >&2
  exit 1
fi

user=$(id -un)
if ! settings=$("$sshd_bin" -T -C "user=$user,host=$client_addr,addr=$client_addr,laddr=$server_addr,lport=$server_port"); then
  echo 'cannot inspect effective hub sshd settings for this connection' >&2
  exit 1
fi
forward=$(printf '%s\n' "$settings" | awk '$1 == "allowstreamlocalforwarding" { print $2; exit }')
unlink=$(printf '%s\n' "$settings" | awk '$1 == "streamlocalbindunlink" { print $2; exit }')
disabled=$(printf '%s\n' "$settings" | awk '$1 == "disableforwarding" { print $2; exit }')
case "$forward" in
  yes|all|remote) ;;
  *) echo "hub sshd denies remote Unix socket forwarding (AllowStreamLocalForwarding=$forward)" >&2; exit 1 ;;
esac
if [ "$unlink" != yes ] || [ "$disabled" != no ]; then
  echo "hub sshd cannot accept this dial-home listener (StreamLocalBindUnlink=$unlink, DisableForwarding=$disabled)" >&2
  exit 1
fi
echo 'Hub sshd permits remote Unix socket forwarding for this connection.'
