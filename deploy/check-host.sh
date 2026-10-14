#!/usr/bin/env bash
# Read-only security check of the machine running plant-server. It changes
# nothing; it reports PASS / WARN / FAIL per item and exits non-zero on FAIL.
#
#   sudo deploy/check-host.sh                  # full check (root sees more)
#   sudo deploy/check-host.sh --lan 192.168.1.1
#       also verify this host can NOT open connections to a LAN machine
#       (DMZ isolation); give the IP of your router or a PC on the LAN
#
# Written for Fedora; most checks also work on Debian/Ubuntu.
set -uo pipefail

CONF=/etc/plant-monitor/config.toml
DATA=/var/lib/plant-monitor
LAN_HOSTS=()
while [[ $# -gt 0 ]]; do
  case "$1" in
    --lan) LAN_HOSTS+=("${2:?--lan needs an IP}"); shift 2 ;;
    -h|--help) sed -n '2,10p' "$0"; exit 0 ;;
    *) echo "unknown option: $1" >&2; exit 2 ;;
  esac
done

if [[ -t 1 ]]; then G=$'\033[32m' Y=$'\033[33m' R=$'\033[31m' B=$'\033[1m' N=$'\033[0m'; else G= Y= R= B= N=; fi
fails=0 warns=0
pass() { printf '  %sPASS%s %s\n' "$G" "$N" "$*"; }
warn() { printf '  %sWARN%s %s\n' "$Y" "$N" "$*"; warns=$((warns + 1)); }
fail() { printf '  %sFAIL%s %s\n' "$R" "$N" "$*"; fails=$((fails + 1)); }
section() { printf '\n%s%s%s\n' "$B" "$*" "$N"; }
have() { command -v "$1" >/dev/null 2>&1; }

[[ $EUID -eq 0 ]] || echo "${Y}Not running as root: some checks are skipped. Use sudo for a full report.${N}"

# ---------------------------------------------------------------- updates
section "System updates"
if have dnf; then
  if [[ $EUID -eq 0 ]]; then
    n=$(dnf -q updateinfo list --security 2>/dev/null | grep -c . || true)
    [[ $n -eq 0 ]] && pass "no pending security updates" || fail "$n pending security updates (dnf upgrade --refresh)"
  fi
  if systemctl is-enabled -q dnf-automatic-install.timer 2>/dev/null || systemctl is-enabled -q dnf5-automatic.timer 2>/dev/null \
     || systemctl is-enabled -q dnf-automatic.timer 2>/dev/null; then
    pass "automatic updates enabled"
  else
    fail "automatic updates not enabled (dnf install dnf-automatic, enable its timer)"
  fi
  if have needs-restarting && [[ $EUID -eq 0 ]]; then
    needs-restarting -r >/dev/null 2>&1 && pass "no reboot needed" || warn "reboot needed to load updated kernel/libraries"
  fi
elif have apt-get; then
  systemctl is-enabled -q unattended-upgrades 2>/dev/null && pass "unattended-upgrades enabled" \
    || fail "automatic updates not enabled (apt install unattended-upgrades)"
  [[ -f /var/run/reboot-required ]] && warn "reboot required" || pass "no reboot needed"
fi

# ---------------------------------------------------------------- MAC / firewall
section "SELinux and firewall"
if have getenforce; then
  [[ $(getenforce) == Enforcing ]] && pass "SELinux enforcing" || fail "SELinux is $(getenforce) (should be Enforcing)"
fi
if systemctl is-active -q firewalld 2>/dev/null; then
  pass "firewalld active"
  if [[ $EUID -eq 0 ]]; then
    zone=$(firewall-cmd --get-default-zone)
    svcs=$(firewall-cmd --zone="$zone" --list-services | tr ' ' '\n' | grep -v '^$' | sort | tr '\n' ' ')
    ports=$(firewall-cmd --zone="$zone" --list-ports)
    extra=$(tr ' ' '\n' <<<"$svcs" | grep -vxE 'ssh|https|http|dhcpv6-client|' | tr '\n' ' ')
    [[ -z $extra && -z $ports ]] && pass "zone $zone allows only: $svcs" \
      || warn "zone $zone also allows: $extra $ports (remove what you don't need)"
  fi
elif have nft && [[ $EUID -eq 0 ]] && nft list ruleset 2>/dev/null | grep -q 'policy drop'; then
  pass "nftables ruleset with drop policy"
else
  fail "no host firewall active (systemctl enable --now firewalld)"
fi

# ---------------------------------------------------------------- listening ports
section "Listening network services"
if have ss; then
  while read -r proto addr proc; do
    port=${addr##*:}; host=${addr%:*}
    case "$host" in 127.*|'[::1]'|'::1') continue ;; esac
    case "$port" in
      22|80|443) pass "$proto $addr $proc" ;;
      8080) fail "$proto $addr $proc: plant-server must listen on 127.0.0.1 only" ;;
      *) warn "$proto $addr $proc: unexpected public listener" ;;
    esac
  done < <(ss -Hlntup 2>/dev/null | awk '{p=$7; sub(/.*\(\("/,"",p); sub(/".*/,"",p); print $1, $5, p}' | sort -u)
fi

# ---------------------------------------------------------------- SSH
section "SSH"
if have sshd && [[ $EUID -eq 0 ]] && cfg=$(sshd -T 2>/dev/null); then
  get() { awk -v k="$1" '$1==k {print $2}' <<<"$cfg"; }
  [[ $(get passwordauthentication) == no ]] && pass "password login disabled" \
    || fail "PasswordAuthentication is $(get passwordauthentication) (use keys only)"
  [[ $(get kbdinteractiveauthentication) == no ]] && pass "keyboard-interactive login disabled" \
    || warn "KbdInteractiveAuthentication is $(get kbdinteractiveauthentication)"
  [[ $(get permitrootlogin) == no ]] && pass "root login disabled" \
    || fail "PermitRootLogin is $(get permitrootlogin) (set no)"
  [[ $(get x11forwarding) == no ]] && pass "X11 forwarding off" || warn "X11Forwarding on"
elif ! systemctl is-active -q sshd 2>/dev/null; then
  pass "sshd not running"
fi

# ---------------------------------------------------------------- accounts
section "Accounts"
uid0=$(awk -F: '$3==0 && $1!="root" {print $1}' /etc/passwd)
[[ -z $uid0 ]] && pass "only root has UID 0" || fail "other UID 0 accounts: $uid0"
if [[ $EUID -eq 0 ]]; then
  empty=$(awk -F: '$2=="" {print $1}' /etc/shadow)
  [[ -z $empty ]] && pass "no accounts with empty passwords" || fail "accounts with empty password: $empty"
fi
shells=$(awk -F: '$3>=1000 && $7!~/(nologin|false)$/ {print $1}' /etc/passwd | tr '\n' ' ')
pass "login users: ${shells:-none} (make sure you know each one)"

# ---------------------------------------------------------------- plant-server
section "plant-server"
if systemctl is-active -q plant-server 2>/dev/null; then
  pass "service running"
  user=$(systemctl show -p User --value plant-server)
  [[ $user == plantmon ]] && pass "runs as unprivileged user plantmon" || fail "runs as '${user:-root}' (should be plantmon)"
  if have systemd-analyze; then
    score=$(systemd-analyze security plant-server 2>/dev/null | awk '/Overall exposure level/ {print $(NF-1)}')
    [[ -n $score ]] && { awk -v s="$score" 'BEGIN{exit !(s<=3.0)}' && pass "systemd sandbox exposure $score (lower is better)" \
      || warn "systemd sandbox exposure $score; was the unit file changed?"; }
  fi
else
  fail "service not running (systemctl status plant-server)"
fi
if [[ -r $CONF ]]; then
  perm=$(stat -c '%a %U:%G' "$CONF")
  [[ $perm == "640 root:plantmon" || $perm == "600 "* ]] && pass "config permissions $perm" || fail "config permissions $perm (want 640 root:plantmon)"
  grep -qE '^device_key *= *"CHANGE-ME' "$CONF" && fail "device_key is still the placeholder" || pass "device_key set"
  grep -qE '^secure_cookies *= *false' "$CONF" && fail "secure_cookies = false (must be true behind HTTPS)" || pass "secure cookies on"
  grep -qE '^listen *= *"(127\.0\.0\.1|\[::1\]):' "$CONF" && pass "listens on loopback only" \
    || { grep -qE '^listen' "$CONF" && fail "listen is not loopback: $(grep -E '^listen' "$CONF")" || pass "listens on loopback (default)"; }
  grep -qE '^device_networks.*0\.0\.0\.0/0' "$CONF" && fail "device_networks allows the whole internet" || pass "device_networks limited to LAN"
  grep -qE '^topic *= *"plants-CHANGE-ME' "$CONF" && warn "ntfy topic is the placeholder"
elif [[ -e $CONF ]]; then
  warn "can't read $CONF (run with sudo)"
else
  warn "$CONF not found (installed elsewhere?)"
fi
if [[ -d $DATA ]]; then
  perm=$(stat -c '%a %U' "$DATA")
  [[ $perm == "750 plantmon" || $perm == "700 plantmon" ]] && pass "data dir permissions $perm" || fail "data dir permissions $perm (want 750 plantmon)"
fi
if have curl; then
  code=$(curl -s -o /dev/null -w '%{http_code}' --max-time 3 http://127.0.0.1:8080/api/plants || true)
  [[ $code == 401 ]] && pass "API refuses unauthenticated requests" || warn "GET /api/plants without login returned '$code' (expected 401)"
fi

# ---------------------------------------------------------------- Caddy
section "Caddy (HTTPS)"
if systemctl is-active -q caddy 2>/dev/null; then
  pass "caddy running"
  [[ $(systemctl show -p User --value caddy) == caddy ]] && pass "caddy runs as user caddy" || warn "caddy not running as user caddy"
  if have curl; then
    hdr=$(curl -skI --max-time 5 https://localhost/ 2>/dev/null || true)
    grep -qi '^strict-transport-security' <<<"$hdr" && pass "HSTS header present" || warn "no HSTS header from https://localhost/"
  fi
else
  fail "caddy not running"
fi

# ---------------------------------------------------------------- misc
section "Other"
timedatectl show -p NTPSynchronized --value 2>/dev/null | grep -q yes && pass "clock synchronized (needed for TLS)" || warn "clock not NTP-synchronized"
if [[ $EUID -eq 0 ]] && have journalctl; then
  n=$(journalctl -u plant-server --since -7d -q 2>/dev/null | grep -c 'failed login' || true)
  [[ $n -lt 20 ]] && pass "$n failed web logins in 7 days" || warn "$n failed web logins in 7 days (someone guessing?)"
  n=$(journalctl -u sshd --since -7d -q 2>/dev/null | grep -cE 'Failed|Invalid user' || true)
  [[ $n -lt 20 ]] && pass "$n failed SSH logins in 7 days" || warn "$n failed SSH logins in 7 days"
fi

# ---------------------------------------------------------------- DMZ isolation
if [[ ${#LAN_HOSTS[@]} -gt 0 ]]; then
  section "DMZ isolation (this host must NOT reach the LAN)"
  for h in "${LAN_HOSTS[@]}"; do
    for p in 22 80 443 445; do
      timeout 3 bash -c "exec 3<>/dev/tcp/$h/$p" 2>/dev/null
      case $? in
        0) fail "connected to $h:$p: the router lets the DMZ reach your LAN" ;;
        124) pass "cannot reach $h:$p (blocked)" ;;
        *) warn "$h:$p answered 'refused': no service there, but the router may let DMZ traffic through" ;;
      esac
    done
  done
else
  printf '\n%sTip:%s add --lan <router or PC IP> to check the DMZ cannot reach your LAN.\n' "$B" "$N"
fi

printf '\n%sResult:%s %d failed, %d warnings\n' "$B" "$N" "$fails" "$warns"
[[ $fails -eq 0 ]]
