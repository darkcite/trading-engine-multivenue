# EC2 deploy kit: slot 11 (pmj) on its own Linux host

This kit runs slot 11 (H90 on Polymarket's 5-minute up/down markets, routed
through Jupiter Predict) on its own engine, on an AWS EC2 host in eu-west-1.
The risk policy requires this: slot 11 arms only alone in its engine, because
a slot-11 boot refusal aborts the whole engine (`docs/risk-policy.md` "PMJ —
slot 11", `### J16`, R-PMJ-S77). The kit installs the engine **PAPER**.
Nothing in it arms anything. Arming is `scripts/pmj-flip.sh live`, after J0,
the prerequisites below and the dated gate line (see "Going live").

**The operator runs every command here.** A session never calls AWS, never
connects over ssh to the host, and never reads, prints or edits `.env`, the
wallet keypair or `s3.env`.

**Venues and region.** The region (eu-west-1) and the AZ are chosen for
latency only. Whether the operator may use Jupiter and Polymarket is the
operator's ruling (R-PM-11). A VM or a region is never a way around a geoblock.
Jupiter refuses US and KR addresses, and `pmj-exec --key-check` names an
`unsupported_region` refusal if it ever meets one.

| file | runs | role |
|---|---|---|
| `az-rtt-probe.sh` | Mac | one throwaway instance per AZ, in its own security group; TCP/TLS handshake RTT to the venues |
| `rtt.sh` | host, probe | the same handshake measurement on the host it runs on |
| `provision.sh` | Mac | the key pair (`ssh-keygen` + import), the instance, security group, Elastic IP and IMDSv2; cloud-init sets UTC, chrony and the core map |
| `host-setup.sh` | host, root, once | packages, user `mv`, the polkit rule, limits, core dumps and kdump off, ptrace locked, no swap, ssh, upgrades, Rust, nextest, uv, units, logs |
| `deploy-linux.sh` | Mac | ships a commit; `install-release.sh` builds it on the host into `/opt/multivenue/<sha>[-git]/` |
| `install-release.sh` | host, root | the host half of a deploy; also `status`, `rollback` and `prune` |
| `host-gates.sh` | host, root | the Linux gate battery for one installed release; verdicts into its `BUILD_INFO` |
| `pmj-wrapper.sh` | host (systemd) | the engine wrapper: `STRATEGY=pmj` only, the two switches, the offline `pmj-check` |
| `pmj-pin.sh` | host (systemd) | moves the engine loop onto the isolated CPU once it runs |
| `pmj-alert.sh` | host (systemd) | the alarm: the marker `state/pmj-alert` and a loud page into `.notify-inbox` |
| `pmj-daily.sh` | host (timer) | the daily lane: the pmj gate, the drain at a safe minute, retention, the claimable listing |
| `pmj-kill.sh` | Mac | halts slot 11 over ssh and confirms it on `/state`; `--stop` stops the engine |
| `systemd/` | host | `multivenue-pmj.service`, `multivenue-pmj-alert.service`; the `-daily` and `-archive` services and timers |
| `env.template`, `strategy.conf.template`, `universe.toml.pmj`, `exec.toml.pmj.example`, `pmj.toml.example`, `retention.conf.pmj` | host | the state root's files, with key names only |
| `check.sh` | Mac or Linux | the kit's static checks: `bash -n`, `zsh -n`, shellcheck, `systemd-analyze verify`, the dry runs, SPDX |
| `BOOT-AUDIT.md` | — | what this engine starts and dials, and the code changes for a cleaner standalone |

## Prerequisites

**On the Mac:**

- AWS CLI v2 with a profile that may manage EC2 and read SSM in eu-west-1;
- `ssh`, `ssh-keygen` and `scp`;
- this repo, with the commit to deploy;
- `shellcheck`, if you want to re-run the checks.

**In AWS:**

- vCPU quota for one c7i.2xlarge (8 vCPU), and three t3.micro for the probe;
- a default VPC in eu-west-1, or pass `--subnet-id`;
- **no EBS snapshot, AMI, AWS Backup plan or Data Lifecycle Manager policy may
  ever cover this instance's root volume.** It holds slot 11's wallet keypair
  and `.env`; a snapshot or an image is a copy of the wallet that outlives the
  instance and is readable by anyone with snapshot rights in the account.
  Never run `create-snapshot` or `create-image` on it, and keep it out of
  every Backup and DLM selection (by tag `Project=multivenue` as well). The
  root volume is deleted with the instance. A broken host is rebuilt from
  step 1, never restored. Check once and after any account-wide backup change:

  ```sh
  aws ec2 describe-snapshots --owner-ids self --filters Name=volume-id,Values=<root-vol> --query 'Snapshots[].SnapshotId'   # []
  aws dlm get-lifecycle-policies --query 'Policies[].{id:PolicyId,tags:TargetTags}'   # none targets Project/Lane/Name of this host
  aws backup list-protected-resources --query "Results[?contains(ResourceArn, '<i>') || contains(ResourceArn, '<root-vol>')]"   # []
  ```

**Accounts and keys, placed later on the host by hand:**

- slot 11's dedicated hot wallet as a Solana CLI keypair file (R-PMJ-S43, S73);
- `JUP_API_KEY`;
- the S3 archive credentials, with a **new** `MULTIVENUE_S3_HOST_ID`, because a different host is a different data population.

**The SSH key is the wallet.** Whoever holds the SSH private key (and its
passphrase) and an admitted IP is `ubuntu`, which has sudo, so they can read
`~mv/keys` and spend slot 11's wallet. Treat the key as the wallet itself:

- give it a passphrase (`provision.sh --create-key` refuses an empty one);
- keep it on this Mac only, never forward it (`ForwardAgent` stays off for `pmj-ec2`);
- keep it out of every backup that leaves the Mac.

**The law that never bends here:** the engine restarts only at a safe minute
(CLAUDE.md "Live safety"; on this host the minute is defined in step 7). Each
step says whether it starts, stops or restarts the engine; none restarts it by
itself.

## Core map (c7i.2xlarge: 4 physical cores × 2 threads)

| CPUs | role |
|---|---|
| 3 | **the engine loop**, alone. It busy-polls. It is isolated: `isolcpus=managed_irq,domain`, `nohz_full`, `rcu_nocbs`. `pmj-pin.sh` moves it here at every start. |
| 7 (3's hyperthread) | isolated. Only the Hyperliquid ingress thread runs here, through the engine's hard-coded pin to core 7. It blocks in epoll, so the loop keeps the physical core. |
| 0-2, 4-6 | housekeeping: the kernel and every IRQ (`irqaffinity`), the unit's `CPUAffinity`, the static-market ingress (pin 1), the Binance ingress (pin 2), `ingress-pm5` and `pm5-discovery`, `metrics-http`, `jup-gw` when live, and the daily, archive, build and gate jobs |

The first boot computes the map from sysfs. It isolates the physical core with
the highest `core_id`, both its threads. It writes
`/etc/multivenue-pmj/cores.env` and the grub arguments, then reboots once.
`host-setup.sh` renders the unit's `CPUAffinity` drop-in from that file.

**Why the unit's `CPUAffinity` is NOT the isolated CPUs.** `isolcpus` turns
load balancing off on those CPUs. With a whole-process mask of {3,7}, every
thread the engine does not pin itself would stay on the CPU it was born on.
That is the loop, slot 11's own feed (`ingress-pm5`, whose hard-coded pin 13
does not exist on 8 vCPUs), discovery, metrics and `jup-gw`. So the process
runs on the housekeeping CPUs, where the scheduler balances it, and only the
main thread is moved onto CPU 3, after its loop runs. A code knob would be
cleaner (BOOT-AUDIT C4).

**Memory.** The engine unit has `OOMScoreAdjust=-900`. Every build and gate
run is inside a `systemd-run --scope` with `MemoryMax` (8G by default,
`PMJ_BUILD_MEMORY_MAX`) at nice 19 and idle IO; the daily and archive units
run at `Nice=19` with `IOSchedulingClass=idle`. If memory runs short, the
kernel kills a build before it touches the engine.

## Step 0: pick the AZ (optional, ~15 min)

```sh
deploy/ec2-pmj/az-rtt-probe.sh --profile <p>            # --dry-run first if you like
```

- **What it makes:** one t3.micro per AZ in a security group of its own (`pmj-az-rtt-probe-<pid>`, no ingress at all), never the engine's group.
- **Check:** a table with one row per AZ and host: TCP and TLS p50/p90 in ms. Pick the AZ with the lowest TLS p50 to `api.jup.ag` and the Polymarket hosts.
- **Rollback:** nothing stays behind. The script terminates its instances on exit, waits for them, then deletes its security group. Each instance also runs `shutdown -h +15` with `terminate` behaviour. To confirm:

  ```sh
  aws ec2 describe-instances --filters Name=tag:Purpose,Values=az-rtt-probe --query 'Reservations[].Instances[].State.Name'
  aws ec2 describe-security-groups --filters Name=tag:Purpose,Values=az-rtt-probe --query 'SecurityGroups[].GroupId'   # delete any left
  ```

A handshake RTT is not the latency law. The law (CLAUDE.md "Venue latency is
measured, never assumed") is `latency_probe`, per host AND per location. It
runs on the host itself after step 4 (see "Latency").

## Step 1: provision (Mac)

```sh
deploy/ec2-pmj/provision.sh --az eu-west-1b --create-key multivenue-pmj --profile <p> --dry-run
deploy/ec2-pmj/provision.sh --az eu-west-1b --create-key multivenue-pmj --profile <p>
```

- **The key (`--create-key NAME`):**
  - It runs `ssh-keygen -t ed25519 -a 100 -f ~/.ssh/NAME` on the Mac. It asks for a passphrase and refuses an empty one.
  - Then it runs `aws ec2 import-key-pair` with the **public** half only.
  - The private key is made on the Mac and never leaves it. AWS never sees it.
  - To use a key pair you already have, pass `--key-name <existing>` instead. It must hold to the same rules.
- **SSH source:** `--my-ip` sets the one address SSH admits. By default it is detected.
- **The security group:**
  - Every re-run reconciles it to exactly one ingress rule: tcp 22 from your /32.
  - It revokes every other ingress rule, whatever its kind: IPv6 ranges, port ranges that span 22, protocol `-1`, prefix lists, group sources.
  - It then checks that the instance carries this group and no other.
- **Re-runs are safe:** a re-run finds everything by its tags and only fixes drift, such as a new IP or the protections.
- **Check:**
  - The script prints the instance, the EIP and the ssh-config stanza (`IdentityFile ~/.ssh/NAME`, `UseKeychain`, `AddKeysToAgent`). Paste the stanza into `~/.ssh/config`; the alias is `pmj-ec2`.
  - Wait about 3 minutes (one reboot). Then compare the host-key fingerprints with the console's copy, using the command it prints. Only then `ssh pmj-ec2`.
  - `ssh pmj-ec2 'cat /proc/cmdline; cat /sys/devices/system/cpu/isolated /etc/multivenue-pmj/cores.env; timedatectl | grep zone; chronyc -n sources'` should show the `isolcpus`/`nohz_full`/`rcu_nocbs` arguments, `3,7`, `Etc/UTC`, and `^* 169.254.169.123`.
- **Rollback:** delete by hand, in order. There is no teardown script.

  ```sh
  aws ec2 modify-instance-attribute --instance-id <i> --no-disable-api-termination
  aws ec2 terminate-instances --instance-ids <i>        # the encrypted root goes with it
  aws ec2 release-address --allocation-id <eipalloc>    # after the instance is gone
  aws ec2 delete-security-group --group-id <sg>         # multivenue-pmj-ssh
  aws ec2 delete-key-pair --key-name multivenue-pmj     # AWS's copy of the PUBLIC key
  rm ~/.ssh/multivenue-pmj ~/.ssh/multivenue-pmj.pub    # if --create-key made them
  ```

## Step 2: host setup (host, root, once)

```sh
scp -r deploy/ec2-pmj pmj-ec2:/tmp/
ssh pmj-ec2 'sudo bash /tmp/ec2-pmj/host-setup.sh'
```

**What it adds:**

- **The user `mv`:** no password, no sudo, and no lingering, because every unit here is a system unit with `User=mv`.
- **One polkit rule** (`/etc/polkit-1/rules.d/50-multivenue-pmj.rules`): `mv` may `try-restart` `multivenue-pmj.service` and nothing else. It may not `restart` (which would start a stopped unit), `start` or `stop` it. The daily drain needs it (step 7).
- **Crash dumps and ptrace:**
  - `kdump-tools` is purged if present, because a crash dump would hold the signer's page.
  - `kernel.yama.ptrace_scope = 2`: only `CAP_SYS_PTRACE` attaches. Not even `mv` can read its own engine's memory.
- **The tools and the logs:**
  - `cargo-nextest` 0.9.148 is installed for the gates. The rustup and uv installers are pinned by sha256.
  - The logs under `~mv/multivenue/logs/systemd/` are pre-created `mv:mv 0600`.
  - logrotate runs `copytruncate` as `mv`.

**Check:** its closing lines.

- The timezone is UTC, and chrony has selected 169.254.169.123.
- `core_pattern` is `|/bin/false`, `suid_dumpable` 0, `ptrace_scope` 2, `kdump-tools absent`, with no swap.
- sshd shows `passwordauthentication no` and `permitrootlogin no`.
- The polkit rule is in place, and `loginctl show-user mv -p Linger` is `no` (or no user session).
- The units are installed. The engine is neither enabled nor started until step 6.

**Then from a second terminal:** `ssh pmj-ec2 true` still works. Keep the first session open until it does.

**Reboot if asked:** if it says *REBOOT REQUIRED*, run `ssh pmj-ec2 sudo systemctl reboot`.

**Rollback:** each item is a file it names, under:

- `/etc/sysctl.d`, `/etc/security/limits.d`, `/etc/systemd/...`, `/etc/logrotate.d`;
- `/etc/ssh/sshd_config.d/01-multivenue.conf`;
- `/etc/apt/apt.conf.d/52multivenue-unattended`, `/etc/needrestart/conf.d`;
- `/etc/polkit-1/rules.d/50-multivenue-pmj.rules`.

Remove the file and reload the service it belongs to, or rebuild the host from step 1.

## Step 3: the wallet directory (host, by hand; live only, skip for paper)

The keypair file lives in `~mv/keys/`, which is 0700. It must be owned by
`mv`, mode exactly 0600, a regular file and not a symlink.

**What refuses where:**

- `pmj_live::load_wallet` refuses a keypair file under the binary's own repo (the release tree: the ancestor of the binary that holds `Cargo.toml` and `crates/`), under `~/multivenue`, or under `MULTIVENUE_DIR`.
- Keeping it out of `~mv/src` as well is this kit's rule.
- The archive unit cannot see `~mv/keys` at all (`InaccessiblePaths`).

**To place it:** copy it there yourself. For example, `scp` it to the ubuntu
user, then `sudo install -o mv -g mv -m 0600 … ~mv/keys/`, then `shred -u` the
copy. The session never sees it.

## Step 4: deploy a release (Mac)

```sh
deploy/ec2-pmj/deploy-linux.sh pmj-ec2 --dry-run
deploy/ec2-pmj/deploy-linux.sh pmj-ec2 --with-git                     # HEAD
deploy/ec2-pmj/deploy-linux.sh pmj-ec2 --with-git --kit-from-worktree # while the kit is uncommitted
```

**Transports, one release directory each:**

- **The default, `git archive <sha>`, is installed as `/opt/multivenue/<sha>`.**
  - It writes nothing to `.git` and is fine for **paper**.
  - The binary reports `boot.git_sha "unknown"` and carries no PMJ gate line, because build.rs needs git (BOOT-AUDIT C8).
  - The wrapper refuses to arm it.
  - `install-release.sh` refuses to make it current, by a deploy or a rollback, while `strategy.conf`'s `ARM_LIVE` names 11 or an `exec*.toml` marks slot 11 live.
- **`--with-git` is installed as `/opt/multivenue/<sha>-git`.**
  - It ships a one-commit shallow repository. The source repo is only read.
  - The binary knows its sha and compiles the committed gate lines.
  - `pmj-flip.sh live` can read `HEAD:docs/risk-policy.md` on the host.
  - **Use it for anything that may arm.**

**What the build does:**

- **Where:** on the host as `mv`, in `~mv/src/<name>` with `CARGO_TARGET_DIR=~mv/build/target`.
- **How hard:**
  - It runs inside a `systemd-run --scope` with `MemoryMax=8G`, at nice 19 with idle IO, `-j 4`. A cold build takes about 15-30 minutes, and a running engine keeps CPUs 3 and 7.
  - It uses `RUSTFLAGS="-C target-cpu=native"`. That replaces the repo's `.cargo/config.toml` x86_64 flags (`znver3`, an AMD target; this CPU is Intel; BOOT-AUDIT C7).
  - The flags rustc actually ran with are read back from the verbose build log into `BUILD_INFO` (`rustflags_observed`). A `znver` there refuses the install.
  - `BUILD_INFO` also records the CPU model; the wrapper refuses to start on another model.
- **The archiver's venv:** `uv sync --frozen --no-dev --no-build --no-install-project`, with Python 3.14 and wheels only, so no source distribution runs code here.
- **What it checks:** `strategy-mask pmj` = `2048<TAB>pmj` and the offline `pmj-exec --self-test`.
- **`.env`:** a release carries none. A source or kit tarball holding one is refused, and every deploy and rollback deletes any `.env` name it finds in a release tree. The one env file is `/home/mv/multivenue/.env`, named by `MULTIVENUE_ENV_FILE` (step 5).

**Check:**

- `deploy-linux.sh pmj-ec2 --status` lists the releases with their `BUILD_INFO`: transport, `rustflags_observed`, `gates_verdict`.
- It also shows `current`, `previous` and what the engine runs.

**Rollback:** `deploy-linux.sh pmj-ec2 --rollback [NAME]` moves `current` back
to `previous`, or to `NAME` (`<sha>` or `<sha>-git`).

- It takes the deploy lock, so it never races a deploy, a prune or a gate run.
- It refuses an archive release while slot 11 is armed.
- The wrapper and units roll back with the binary.
- **Neither a deploy nor a rollback restarts the engine.** It boots the new `current` at its next drain ("Taking a release").

## Step 4b: the Linux gate battery (host, root, after every deploy that may arm)

The Mac's gates say nothing about this binary: another OS, another CPU,
another codegen. `host-gates.sh` runs them on the host, on the release's own
source tree at its sha, with the release's own `RUSTFLAGS`, in a separate
target dir (`~mv/build/gates-target`), at nice 19 inside a `MemoryMax` scope.

- **`HOME` is a scratch directory** (`~mv/build/gates-home`, emptied each run), with `CARGO_HOME` and `RUSTUP_HOME` pinned to mv's toolchain. No test can read or write the live `~mv/multivenue`: its halts, ledgers and `.env`.
- **It takes the deploy lock** (`~mv/build/.deploy.lock`). No deploy, rollback or prune runs during it, and it refuses to start during one.

It runs long (a cold test build). So run it detached, not tied to your ssh
session:

```sh
ssh pmj-ec2 'sudo systemd-run --unit=pmj-host-gates --collect bash /opt/multivenue/current/deploy/ec2-pmj/host-gates.sh'
ssh pmj-ec2 'sudo journalctl -u pmj-host-gates -f'                # or the full log: ~mv/build/gates-<name>-<UTC>.log
ssh pmj-ec2 'grep "^gate" /opt/multivenue/current/BUILD_INFO'
```

**What it runs:**

1. `cargo nextest run` (`cargo test` if nextest is absent) on `-p cli -p exec-jupiter -p signer-ed25519 -p core-config -p core-net -p exec-router`.
2. The allocation gate: `cargo test -p bench --test alloc_assertions --release -- --test-threads=1`, after `cargo clean -p bench --release`. It is green only with a fresh `Compiling bench` in the log and every assertion at 0 B.
3. `multivenue-engine pmj-exec --self-test` on the installed binary.
4. Fuzz is **not run**, because it needs nightly and this host has none. It is recorded as `not run (needs nightly)`. The fuzz gate for that sha stays the Mac's.

**Check:** exit 0 and `gates_verdict=GREEN` in `BUILD_INFO` and in `--status`.

- **A green battery on the release that will arm is a prerequisite before any dust line.**
- It reads no secret, dials no venue (crates.io only, for a cold dependency fetch) and never touches the engine.
- Run it outside a drain. Its test binaries bind loopback ports. Re-run a red alone before you believe it, as on the Mac.

**Rollback:** nothing to roll back. It writes only the `gate_*` lines of
`BUILD_INFO` and its own target dir. `deploy-linux.sh pmj-ec2 --prune --target`
removes the cache.

## Step 5: the state root (host, by hand)

```sh
ssh pmj-ec2
sudo -u mv -H bash
cd ~/multivenue
K=/opt/multivenue/current/deploy/ec2-pmj
install -m 0600 $K/env.template .env                  # then fill the values with an editor, as mv: never a session
install -m 0600 $K/strategy.conf.template strategy.conf
install -m 0600 $K/universe.toml.pmj universe.toml    # then paste ONE token into `markets` (below)
install -m 0600 $K/exec.toml.pmj.example exec.toml
install -m 0600 $K/retention.conf.pmj retention.conf
```

**`.env`:**

- **What goes in it:**
  - **Paper** needs only the four required endpoints. Take the values from `.env.example`.
  - **Live** adds `PMJ_WALLET_KEYFILE` (the absolute path from step 3), `PMJ_WALLET_PUBKEY` and `JUP_API_KEY`.
- **What may not:** it must not set `STRATEGY`, `ARM_LIVE`, `EXEC_TOML` or `PMJ_TOML`, not even empty. The wrapper, `pmj-exec.sh` and `pmj-dust.sh` refuse it (78), and the daily lane's claimable listing will not source it. The switches live in `strategy.conf` alone.
- **One file, one name.** `/home/mv/multivenue/.env` is the only env file on this host. Everything that sources it finds it through `MULTIVENUE_ENV_FILE`:
  - the engine and daily units set it;
  - `pmj-wrapper.sh` and `pmj-daily.sh` default to it;
  - the repo's `pmj-exec.sh`, `pmj-flip.sh` and `pmj-dust.sh` read it before their repo `.env`. Run them with `env MULTIVENUE_ENV_FILE=/home/mv/multivenue/.env …`, as the commands under "Going live" do.

  No release carries a `.env`. Without the variable, those scripts look for one in the release, find none, and refuse (78).
- **The checks, everywhere it is sourced:** an absolute path; never a symlink; a regular file; mode 0600; owned by `mv`; no strategy switch.
- **Check:** `stat -c '%a %U %F' ~mv/multivenue/.env` = `600 mv regular file`.

**`universe.toml`, the static market:**

- The boot law forces one static Polymarket market that slot 11 never trades (BOOT-AUDIT C1).
- A Gamma failure on that token at boot is **fatal**: the boot exits 1 and relaunches with backoff, and every exit pages (BOOT-AUDIT 4d).
- Use the **longest-dated active** crypto up/down market you can find, because nothing refreshes it here. Replace it at a redeploy once it resolves.
- Another kind of placeholder needs Anton's exception (M1-R1).

**`pmj.toml`:** the Mac's file, **without any `guard_*` or `build_extra*`
line**. Guard keys on this host come only from Anton's J0 review here.

```sh
grep -Ev '^[[:space:]]*(guard_|build_extra)' ~/multivenue/pmj.toml > /tmp/pmj.toml   # Mac
scp /tmp/pmj.toml pmj-ec2:/tmp/pmj.toml && rm /tmp/pmj.toml                           # Mac
ssh pmj-ec2 'sudo install -o mv -g mv -m 0600 /tmp/pmj.toml ~mv/multivenue/pmj.toml && rm /tmp/pmj.toml'
ssh pmj-ec2 "sudo grep -c '^\s*\(guard_\|build_extra\)' ~mv/multivenue/pmj.toml"     # MUST print 0
```

The paper law is the same as main's, but the file is not byte-identical, so the
boot-tell hash differs. Compare the boot tell's law string with main's
instead. `pmj.toml.example` documents each key, the R-PMJ-S22 paper frame and
LAW PMJ-G. Empty allowlists refuse every live action until J0.

**Check:** as `mv`:

```sh
/opt/multivenue/current/target/release/multivenue-engine pmj-check
```

It prints `rolling=updown:btc:5m,…` and the boot tell. An empty `markets`
refuses here with "no Polymarket markets configured". The wrapper runs the same
check before every start.

**Rollback:** remove the files. Nothing runs yet.

## Step 6: first start, paper (host)

```sh
ssh pmj-ec2 'sudo systemctl enable --now multivenue-pmj.service'
```

**Check:**

- `systemctl status multivenue-pmj` is active. `~mv/multivenue/logs/systemd/engine.err.log` shows the wrapper's lines (the release, `STRATEGY=pmj — mask 2048`, `pmj-check: …`) and then the engine's.
- `pmj-pin: engine loop (pid …) pinned to CPU 3` appears, followed by the thread table: `ingress-pm5` on `0-2,4-6`, `hyperliquid` on `7`, the main thread on `3`.
- Through the tunnel (below), `/state`:
  - `boot.paper` 1;
  - `boot.strategy` `pmj`;
  - `slots[11].enabled` 1;
  - the run dir under `~mv/multivenue/logs/run-*`.

  `vm_rows_active` does **not** apply here: no vm member.
- Slot 11's paper decisions appear in the boot tell and on `/state`, as on main.

**If it fails:**

- **The wrapper refused** (status 78: a configuration error, including a failing `pmj-check`):
  - The unit ends *failed* and does **not** relaunch.
  - `multivenue-pmj-alert.service` writes the marker and a page.
  - The reason is the last line of `engine.err.log`. Fix it, then `sudo systemctl start multivenue-pmj.service`.
- **The engine itself exited non-zero** (a boot refusal, a crash):
  - systemd relaunches it after 15 s, backing off to 5 minutes over six steps.
  - The first exit pages; further pages are held for 15 minutes.

**Rollback:** `sudo systemctl disable --now multivenue-pmj.service`, which is the SIGTERM drain. It stays stopped.

## Step 7: the daily lane and the archive timer (host)

```sh
ssh pmj-ec2 'sudo systemctl enable --now multivenue-pmj-daily.timer multivenue-pmj-archive.timer'
```

**The 0020 slot.** `pmj-daily.sh` ticks at second 30 of every minute. Once per
UTC day, at the first tick at or after 00:20Z, it owes a drain (the day's run
dir closes, so retention can archive it).

**The safe minute** is second 30 of a minute `m` with `m mod 5 = 1`:
00:21:30, 00:26:30, 00:31:30 and so on.

- **The 5-minute windows:** a window opens at second 0 of every `m mod 5 = 0` minute, so the drain starts 90 s into one.
  - Slot 11 decides at τ ∈ [6, 20) s before a window's end (R-PMJ-S55). The previous window's last possible submit is therefore 96 s before the drain.
  - It needs the next window's strike reports over [open − 62, open − 2] s (R-PMJ-S61), 238-298 s into this window. They begin 148 s after the drain starts. The stop (60 s at most before SIGKILL) and a ~20-40 s boot are back well before that.
- **The 15-minute grid:** every drain stays at least 90 s off the :00/:15/:30/:45 ends. Minutes 1, 16, 31 and 46 start 90 s after one; minutes 11, 26, 41 and 56 finish at least 90 s before the next.
- **The late check:** the clock is re-read right before the restart. If the gate ran past second 50, the drain waits for the next safe minute.
- **Orders in flight:** with slot 11 live (`boot.paper` 0), the drain first reads `/metrics`.
  - The arm's `engine_pmj_arm_submitted_total − engine_pmj_arm_done_total` is the number of its orders not yet retired. Every order the arm opens retires exactly once into `done`: refused, filled or closed, released, or lost.
  - If it is above 0, or unreadable while live, the drain waits for the next safe minute. This is logged and is not a gate failure. After 12 waits in a row (an hour), it alerts once.
  - jup-gw's `landed − done − lost` is not used. An in-doubt order that lands later counts in both `in_doubt` and `landed`, and one that never lands is released into `done` without landing.
- **Every drain costs the current window.** The restarted engine never trades a window whose strike it did not see, and this window's strike was reported before the drain. It trades from the next window on: one 5-minute window a day, plus one per forced restart.

**The drain:**

- It is `systemctl try-restart --no-block` as `mv`, through the polkit rule: SIGTERM, the drain, 60 s before SIGKILL, then a boot of whatever `current` names.
- A manual restart resets systemd's restart counter, so the crash backoff never stretches the daily dark window.
- Without polkit it falls back to SIGTERM on the unit's MainPID and logs that the counter grows.
- `try-restart` never starts a stopped unit (`pmj-kill.sh --stop` stays stopped).
- Then it waits for a new `boot.pid` on `/state` and logs the boot. No boot within 5 minutes pages.

**When armed, it runs the J16 pmj gate first:**

- An `exec.HALT` naming 11 defers the drain without a check.
- Otherwise `pmj-exec.sh self-test`, then `key-check` at the slot's cap, reading the env file through `MULTIVENUE_ENV_FILE`.
- 77 (a review trigger) appends `slot=11 reason=operator` to the `exec.HALT` in force and pages.
- Any other failure defers with a 1 / 5 / 15 / 60-minute backoff, and writes `state/pmj-gate-RED` and pages after three.

**Not ported, and why:** E3's `exec-smoke` gate proves the Hyperliquid
EIP-712 signer. This engine can arm no Hyperliquid or Binance slot: the
wrapper refuses any `STRATEGY` but `pmj`, and the engine refuses a live slot
beside 11. The pmj gate is this engine's E3.

**After the drain:**

- the retention pass, gated on `verify`;
- the read-only listing of claimable positions. Claims are never sent.

**The first tick seeds the stamp without firing.**

**Check:**

- `tail ~mv/multivenue/logs/systemd/daily.log` and `systemctl list-timers 'multivenue-pmj*'`.
- The day after, the log shows `slot 0020 fired`, then at 00:21:30 the drain `by systemctl try-restart (polkit)`, then `booted: pid=… git_sha=… paper=1 …`. When live, an `in-flight check` line comes before the drain.
- `systemctl show -p NRestarts multivenue-pmj` stays 0 after a drain.

**Rollback:** `sudo systemctl disable --now multivenue-pmj-daily.timer multivenue-pmj-archive.timer`.

## Step 8: the cold archive (S3, optional)

1. As `mv`, create `~/multivenue/s3.env` from the repo's `s3.env.example`, mode 0600. Set a NEW `MULTIVENUE_S3_HOST_ID`, for example `ec2-pmj-euw1`. Only the Python loader reads this file (S-LAW 6, S-DOCTRINE: handwritten SigV4, no SDK, nothing in the engine). The archive unit cannot see `.env` or `~mv/keys`.
2. Take the first push by hand, as `mv`:

   ```sh
   ~/multivenue/venv/bin/python3 /opt/multivenue/current/scripts/archive-run.py push-pending --budget-s 3600
   ```

   The alias `~/multivenue/venv` appears after the first archive-cycle run (00:40Z). Until then, use `/opt/multivenue/current/claude-worker/.venv/bin/python3`.
3. Run `… archive-run.py verify <run-…>`. It should exit 0.
4. Only then set `ARCHIVE_MODE="s3"` in `~/multivenue/retention.conf`. From that night on the local disk keeps 24 h (PROTECT_DAYS 1) and each delete follows a `verify`.

**Cost note:** egress from AWS to a non-AWS bucket is billed per GB.

**Rollback:** set `ARCHIVE_MODE="compress"`. Nothing in the bucket is deleted by anything here.

## Alerts, and the relay this host does not have

**What rings on the host:**

- **Every engine exit that is not a clean drain or stop** (`ExecStopPost=pmj-alert.sh exit`): a line in the marker; a loud page, at most once per 15 minutes.
- **The unit ending FAILED** (`OnFailure=multivenue-pmj-alert.service`: a wrapper refusal, a start timeout): the marker and a loud page, always.
- **The daily lane** (`pmj-alert.sh note`): a review trigger (77), gate RED, a drain with no boot, an hour of drain waits (orders in flight, or `/metrics` unreadable while live). The `exec.HALT` deferral pages silently, once per halt.

**Where it lands:**

- the marker: `~mv/multivenue/state/pmj-alert`, one line per alarm, newest last, 0600;
- the pages: `~mv/multivenue/.notify-inbox/*.msg`, in `scripts/notify-send.sh`'s format.

**Nothing on this host sends either anywhere.** Notifications for slot 11 are
"Not now" (R-PMJ-S75).

**HARD PREREQUISITE before any dust line: a relay must exist and be tested.**
Which relay is **an open ruling for Anton**:

- **(a) `multivenue-notify` on this host**, under N-DOCTRINE.
  - This needs Anton to lift R-PMJ-S75 for slot 11.
  - It also needs a build of `-p notify`, a system unit, and `TELEGRAM_BOT_TOKEN` / `TELEGRAM_CHAT_ID` in its own environment. The kit does not build or install any of these today.
- **(b) A Mac-side watcher over SSH** that polls the marker's size and the inbox and forwards new lines into the Mac's own inbox with `scripts/notify-send.sh -l -s PMJ-EC2`, so main's notifier relays them.
  - It must also page when ssh or `/state` stops answering (the host down is the alarm the host cannot send).
  - It must never use local port 9191.

Either way, the test is to force one page (`sudo -u mv -H /opt/multivenue/current/deploy/ec2-pmj/pmj-alert.sh note "relay test"`) and see it arrive. Until then, `daily.log`, `state/pmj-alert` and `state/pmj-gate-RED` are the record. Watch them.

## Day to day

**`/state` and the dashboard.** Use a tunnel, and **never on local port 9191**: the Mac's 9191 is main's engine (ONE ENGINE: its 9191 is a singleton). A tunnel there would collide with main, or capture main's port at its next restart.

```sh
ssh -N -L 19191:127.0.0.1:9191 pmj-ec2
curl -s 127.0.0.1:19191/state | jq '{boot: .boot | {pid, git_sha, paper, strategy}, slot11: .slots[11], halt11: .exec.halted[11]}'
# main's dashboard, a second instance aimed at the tunnel (its own port, never 9292):
CLAUDE_WORKER_ENGINE_URL=http://127.0.0.1:19191 CLAUDE_WORKER_DASHBOARD_PORT=9294 …   # scripts/dashboard.sh's command line
```

**Status:**

- `deploy-linux.sh pmj-ec2 --status` (releases, gates, what runs);
- `ssh pmj-ec2 sudo -u mv -H /opt/multivenue/current/scripts/pmj-flip.sh status`;
- `ssh pmj-ec2 'tail -n 20 /home/mv/multivenue/state/pmj-alert'`.

**Latency.** CLAUDE.md's law: venue latency is measured per host AND per
location, with `latency_probe`, before any backtest number is trusted for that
box. Run it on the host, as `mv`, for 20 minutes (stdlib only):

```sh
ssh pmj-ec2 'sudo -u mv -H env PYTHONPATH=/opt/multivenue/current/claude-worker/src /opt/multivenue/current/claude-worker/.venv/bin/python3 -m claude_worker.latency_probe --out /home/mv/latency-$(date -u +%Y%m%d) --minutes 20 --venues polymarket,hyperliquid,binance,binance-usdm'
```

Take `summary.json` into `docs/venue-latency.md` (location: EC2 eu-west-1, the
AZ) by hand. `rtt.sh` adds the TCP/TLS handshakes to the hosts the probe has no
row for: `api.jup.ag`, Gamma, the Polymarket WS hosts, Solana RPC. Run it with
`ssh pmj-ec2 /opt/multivenue/current/deploy/ec2-pmj/rtt.sh`.

**Halt slot 11 now:** `deploy/ec2-pmj/pmj-kill.sh pmj-ec2`.

- **What it does:** it appends `slot=11 reason=operator` to the host's `exec.HALT`, beside the `EXEC_TOML` in force, unless a line there already halts 11. Then it polls `/state` for about 5 s.
- **Exit 0:**
  - slot 11 is live (`boot.paper` 0) and `exec.halted[11]` is no longer `none`, which is the halt confirmed;
  - or slot 11 is not live, so nothing can reach a venue, and the line stands for the next armed boot, which refuses to arm 11 while it is there.
- **Exit 1:** not confirmed within 5 s, `/state` not answering, or the line not written. **Then run `--stop`.**
- **`--stop`:**
  - It halts, then runs `sudo systemctl stop`: SIGTERM, the drain, 60 s before SIGKILL. It checks that the unit is inactive.
  - It stays stopped until `systemctl start`, or until the host reboots (the unit is enabled). `sudo systemctl disable --now multivenue-pmj.service` keeps it down across reboots.
- **`--state`:** only shows the state.
- **Clearing it:** after review, archive the line by hand, or run `pmj-flip.sh paper`.

**When ssh itself is refused** (your IP changed: the group admits one /32).
From any shell with your AWS credentials:

```sh
aws ec2 stop-instances --region eu-west-1 --instance-ids <i>   # ACPI shutdown: systemd stops the unit (SIGTERM, the drain)
deploy/ec2-pmj/provision.sh --az <az> --key-name <key> --my-ip <new-ip>   # re-admit SSH
```

Termination protection does not block a stop. Starting the instance again boots
the engine as `strategy.conf` says, armed if `ARM_LIVE` names 11. Run
`pmj-kill.sh` (or `pmj-flip.sh paper`) right after `start-instances`.

**Taking a release (a restart).** Restarts happen only at a safe minute,
through the gate, as on main. Never restart with a bare `systemctl restart`.
Force the daily slot instead:

```sh
ssh pmj-ec2 'sudo -u mv -H sh -c "echo 19700101 > /home/mv/multivenue/state/last-restart-utc-0020"'
```

It drains at the next safe minute after 00:20Z. That is at most 5 minutes
away, unless the gate or an order in flight defers it, and `daily.log` says
why. The drain costs the current 5-minute window.

**Pruning:** `deploy-linux.sh pmj-ec2 --prune` deletes old releases from
`/opt/multivenue` and `~mv/src`.

- It always keeps `current`, `previous` and the release the engine is running, even when that is neither.
- `--prune --target` also drops the build caches; the next build and gate run are cold.
- Delete nothing there by hand.

## Going live (not this kit's step)

What stands between this host and a live slot 11 is the risk policy's list
(J16 "Open before the dust line"), plus this host's own prerequisites:

1. **J0 on this host:** the Jupiter-facing probes run from here, since the region is allowed.
2. **The rulings:**
   - the `guard_*` keys from Anton's decode, on this host;
   - R-PMJ-S44, S45, S46, S48;
   - R-PMJ-S76..S78;
   - requirement (d), the market hash bound to the window;
   - J15's limits;
   - **the relay** (a) or (b) above, built and tested.
3. **A `--with-git` release** built after the dated line is committed, with **`host-gates.sh` GREEN** on it.
4. **No snapshot, AMI, Backup or DLM policy** over the root volume.
5. **The dated `PMJ-DUST-GATE` line**, committed by Anton.
6. **The dust battery**, as `mv` on the host. Its touch-at-decision and D record replace the dropped P1 survival capture.
7. **`pmj-flip.sh live`**, as `mv`, on the host.
8. **Forcing the slot.**

**The operator commands, each with the one env file** (as `mv`; `R=/opt/multivenue/current`):

```sh
ssh pmj-ec2
sudo -u mv -H bash
R=/opt/multivenue/current; E=/home/mv/multivenue/.env
env MULTIVENUE_ENV_FILE=$E $R/scripts/pmj-exec.sh key-check --cap-day-usd-1e6 50000000      # the read-only key check
env MULTIVENUE_ENV_FILE=$E $R/scripts/pmj-dust.sh --battery <ID> --orders <N> --path execute --dry-run
env MULTIVENUE_ENV_FILE=$E $R/scripts/pmj-dust.sh --battery <ID> --orders <N> --path execute
env MULTIVENUE_ENV_FILE=$E $R/scripts/pmj-flip.sh live
$R/scripts/pmj-flip.sh status                                                             # reads no env file
```

The dust battery is built only into a binary that carries the dated line, so
it needs a `--with-git` release deployed after that commit, with gates GREEN.

**What the flip refuses:**

- another live slot;
- no committed line;
- the halt naming 11;
- an unpinned `guard_program_slot`;
- a failing `pmj-check` or key check at the $50 cap.

**The restart:** the next drain arms slot 11 through the pmj gate.

**Laws to keep in mind:**

- A crash relaunch (`Restart=always`) skips the gate, as launchd's KeepAlive does on the Mac. It backs off 15 s → 5 min and pages.
- A slot-11 boot refusal by the engine exits 1 and relaunches with that backoff until `pmj-flip.sh paper` or a wallet sweep. That is why slot 11 runs alone here.
- A wrapper refusal (78) does not relaunch.
- Requirement (a) is judged at boot. Move no funds in during a session.
