# Seoul provisioning plan

These commands are a plan for the parent operator. Provisioning has not been
executed. The proposed resources are `latch-relay-seoul-1`, its static IP
`latch-relay-seoul-1-ip` and a named imported SSH public key. The current
EC2 backup relay is managed separately.

## Check the account, price, zone and key

Use AWS CLI v2 with the restored session. Confirm account `043189681495` before
any creation. Choose `small_3_0` or `micro_3_0` after the workload inspection.
The default below proposes the $12 bundle. The parent makes the final choice.

```bash
set -euo pipefail
export AWS_REGION=ap-northeast-2
export RELAY_NAME=latch-relay-seoul-1
export RELAY_BUNDLE=small_3_0
export RELAY_ZONE=ap-northeast-2a
export RELAY_KEY=latch-relay-management
: "${RELAY_ADMIN_CIDR:?set the operator public IPv4 /32}"
export RELAY_ADMIN_CIDR
test "$(aws sts get-caller-identity --query Account --output text)" = 043189681495
python3 - <<'PY'
import ipaddress,os
network=ipaddress.ip_network(os.environ['RELAY_ADMIN_CIDR'], strict=True)
assert network.version == 4 and network.prefixlen == 32
assert os.environ['RELAY_BUNDLE'] in ('small_3_0','micro_3_0')
PY
# shellcheck disable=SC2016 # Backticks below are JMESPath literal syntax.
aws lightsail get-bundles --region "$AWS_REGION" \
  --query 'bundles[?bundleId==`small_3_0` || bundleId==`micro_3_0`].{Id:bundleId,USD:price,RAM:ramSizeInGb,CPU:cpuCount,GB:transferPerMonthInGb,Active:isActive}'
# shellcheck disable=SC2016
aws lightsail get-regions --region "$AWS_REGION" --include-availability-zones \
  --query 'regions[?name==`ap-northeast-2`].availabilityZones'
# shellcheck disable=SC2016
aws lightsail get-blueprints --region "$AWS_REGION" \
  --query 'blueprints[?blueprintId==`ubuntu_24_04`].{Id:blueprintId,Active:isActive}'
aws lightsail get-key-pairs --region "$AWS_REGION" \
  --query 'keyPairs[].{Name:name,Fingerprint:fingerprint}'
```

If the named key is absent, import the existing RSA management public key. The
deployment accepted RSA SSH text with the `file://` form below. Ed25519 was
rejected, and wrapping the SSH text in base64 also failed. The CLI must succeed before
creating an instance. If an identically named key exists, verify its fingerprint
against the operator's intended key before reuse. The import below takes only
the public file.

```bash
aws lightsail import-key-pair --region "$AWS_REGION" \
  --key-pair-name "$RELAY_KEY" \
  --public-key-base64 file:///Users/daniel/.ssh/latch6_lightsail_rsa.pub
```

## Create or reuse tagged resources

The first block reads before creating and verifies an existing instance's
bundle, zone, key and ownership tags. A mismatched resource requires inspection.
Any CLI error aborts the plan. It cannot be treated as a missing resource.
Wait for each returned operation to reach `Succeeded` through
`aws lightsail get-operation --operation-id ID` before continuing.

```bash
count=$(aws lightsail get-instances --region "$AWS_REGION" \
  --query "length(instances[?name=='$RELAY_NAME'])" --output text)
if [[ $count == 0 ]]; then
  aws lightsail create-instances --region "$AWS_REGION" \
    --instance-names "$RELAY_NAME" --availability-zone "$RELAY_ZONE" \
    --blueprint-id ubuntu_24_04 --bundle-id "$RELAY_BUNDLE" \
    --key-pair-name "$RELAY_KEY" --ip-address-type ipv4 \
    --tags key=Project,value=Latch key=Purpose,value=native-tcp-relay key=Release,value=6.0.0
else
  aws lightsail get-instance --region "$AWS_REGION" --instance-name "$RELAY_NAME" \
    --output json | python3 -c '
import json,os,sys
i=json.load(sys.stdin)["instance"]
t={v["key"]:v["value"] for v in i.get("tags",[])}
assert t.get("Project")=="Latch" and t.get("Purpose")=="native-tcp-relay"
assert i["bundleId"]==os.environ["RELAY_BUNDLE"]
assert i["location"]["availabilityZone"]==os.environ["RELAY_ZONE"]
assert i["sshKeyName"]==os.environ["RELAY_KEY"]
print("Verified existing instance",i["name"],i["state"]["name"])'
fi
aws lightsail get-instance-state --region "$AWS_REGION" --instance-name "$RELAY_NAME"
```

Create the static IP once. Retain the allocation operation ID as proof of
ownership. Reuse is safe when it is already attached to this instance. An
existing unattached name requires checking the allocation record before use.
The instance carries the ownership tags. Lightsail static IPs have no tags field.

```bash
count=$(aws lightsail get-static-ips --region "$AWS_REGION" \
  --query "length(staticIps[?name=='$RELAY_NAME-ip'])" --output text)
if [[ $count == 0 ]]; then
  aws lightsail allocate-static-ip --region "$AWS_REGION" --static-ip-name "$RELAY_NAME-ip"
fi
attached=$(aws lightsail get-static-ip --region "$AWS_REGION" \
  --static-ip-name "$RELAY_NAME-ip" --query staticIp.attachedTo --output text)
if [[ $attached == None ]]; then
  aws lightsail attach-static-ip --region "$AWS_REGION" \
    --static-ip-name "$RELAY_NAME-ip" --instance-name "$RELAY_NAME"
elif [[ $attached != "$RELAY_NAME" ]]; then
  echo 'Static IP belongs to another instance. Inspect ownership.' >&2
  exit 1
fi
aws lightsail get-static-ip --region "$AWS_REGION" --static-ip-name "$RELAY_NAME-ip" \
  --query 'staticIp.{Name:name,IP:ipAddress,Attached:attachedTo}'
```

Apply the exact firewall to this verified new dedicated instance. This replaces
its public inbound rules, so repeat the instance ownership check when resuming
the plan in another shell. TCP 80 can be added for HTTP-01 if the parent chooses
that certificate method. The proposed DNS-01 setup uses TCP 443 and restricted
SSH. Keep the current SSH session until a second key-based session works.

```bash
ports=$(python3 - <<'PY'
import json,os
print(json.dumps([
 {'fromPort':22,'toPort':22,'protocol':'tcp','cidrs':[os.environ['RELAY_ADMIN_CIDR']]},
 {'fromPort':443,'toPort':443,'protocol':'tcp','cidrs':['0.0.0.0/0']}
]))
PY
)
aws lightsail put-instance-public-ports --region "$AWS_REGION" \
  --instance-name "$RELAY_NAME" --port-infos "$ports"
aws lightsail get-instance-port-states --region "$AWS_REGION" --instance-name "$RELAY_NAME"
```

Connect as Ubuntu's `ubuntu` user with the imported private key. Verify the
host key through an authenticated AWS console path before accepting it. Install
only the required OS tools and native relay binary, run the loopback binary
selftest on that CPU, then follow `README.md`. Set the parent-owned DNS record
and issue the public certificate before the first public service start.

## Reboot, recovery and sustained-load checks

Run a real host/viewer session, record the quota's current reserved bytes and
check metrics. During an arranged interruption reboot the dedicated instance.

```bash
aws lightsail reboot-instance --region "$AWS_REGION" --instance-name "$RELAY_NAME"
aws lightsail get-instance-state --region "$AWS_REGION" --instance-name "$RELAY_NAME"
python3 deploy/relay-tcp/selftest.py --live relay.latch.rune.foo
```

Wait for reboot completion before the public probe. Check SSH and
`systemctl is-enabled/is-active latch-relay.service`, effective `tc qdisc show`,
BBR, unchanged idle quota, and an actual reconnect with a new grant and keys.
For process recovery, use a controlled `systemctl kill --signal=SIGKILL
latch-relay.service` during the test window, wait beyond 30 seconds and verify
`NRestarts`, retained reservations and a new paired session. A live TLS probe
checks the public certificate and ALPN. Whole-session recovery needs the paired
application test.

Fetch sustained-load metrics with explicit UTC timestamps. `CPUUtilization`
includes `Average`/`Maximum`; `BurstCapacityPercentage` and `BurstCapacityTime`
show whether testing is consuming a temporary reserve. Repeat the command for
each metric. Inspect memory and process CPU with `inspect.sh` and systemd.

```bash
: "${RELAY_TEST_START:?ISO8601 UTC start}"
: "${RELAY_TEST_END:?ISO8601 UTC end}"
aws lightsail get-instance-metric-data --region "$AWS_REGION" \
  --instance-name "$RELAY_NAME" --metric-name BurstCapacityPercentage \
  --period 300 --start-time "$RELAY_TEST_START" --end-time "$RELAY_TEST_END" \
  --unit Percent --statistics Average Minimum
```

Do a fresh loopback selftest with the shipped binary before reopening service
after CPU/OS changes. Preserve `/var/lib/latch-relay/quota` across repairs. A
storage failure must be repaired from the current durable copy. Snapshot recovery
can restore an older reservation count, so reconcile quota before public start.
Take any paid snapshot only after the parent chooses that recovery policy.

## Cleanup or failed provisioning

Record created names and operation IDs during execution. On a failed step,
inspect those exact names and operations, fix the failed prerequisite and rerun
from the corresponding read-before-create block. Creation and static-IP
attachment are asynchronous. A second invocation before completion needs the
original operation result first. An unused IP allocation starts billing after
AWS's grace period, so attach or release it promptly.

Deletion is an explicit parent operation after moving DNS, draining sessions,
preserving the current quota and verifying ownership tags. These commands are
destructive and are included for the planned new resources only.

```bash
aws lightsail get-instance --region "$AWS_REGION" --instance-name "$RELAY_NAME" \
  --query 'instance.{Name:name,Tags:tags,State:state.name}'
aws lightsail get-static-ip --region "$AWS_REGION" --static-ip-name "$RELAY_NAME-ip" \
  --query 'staticIp.{Name:name,Attached:attachedTo}'
# After checking the exact resources and saving the current ledger:
aws lightsail detach-static-ip --region "$AWS_REGION" --static-ip-name "$RELAY_NAME-ip"
# Wait for the detach operation to succeed, then release the unused address.
aws lightsail release-static-ip --region "$AWS_REGION" --static-ip-name "$RELAY_NAME-ip"
aws lightsail delete-instance --region "$AWS_REGION" --instance-name "$RELAY_NAME"
```

Verify deletion operations reached `Succeeded`. Inspect the exact related
snapshots and key-pair use before deleting either. Deleting an instance leaves
snapshots billing and does not reset the region's pooled monthly transfer usage.
Stopping an instance continues its bundle charges.
