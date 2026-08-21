# OpenWrt Lite lifecycle tests

These dependency-free tests describe the resource lifecycle expected from the
OpenWrt Lite package.  They cover:

- owned `tinc_*`, `ovpn_*` and `wg_*` OpenWrt interfaces;
- one shared firewall zone without unconditional network/firewall reloads;
- last-known-good recovery after power loss;
- rejection of OpenClash's `utun`, user devices and user UCI sections;
- runtime stop of Meduza-owned processes/links while preserving restart data;
- purge-time removal of only Meduza-owned UCI sections and generated files.
- non-sensitive generator stage diagnostics for otherwise silent `set -e`
  failures;
- cache recovery safety, preservation of a locally committed LKG when the
  etcd acknowledgement fails, and acknowledgement retry without re-applying.

The persistent ownership contract is
`/etc/meduza/managed/interfaces` with five tab-separated columns
(`kind`, `instance`, `logical`, `device`, `config`).  Atomic reconciliation may
use `/etc/meduza/managed/interfaces.pending`.  The last-known-good bundle is
`/etc/meduza/cache.json`.

Run them from the repository root:

```sh
sh alternative/openwrt-lite/tests/run.sh
```

or directly:

```sh
python3 -m unittest -v alternative/openwrt-lite/tests/test_lifecycle.py
```

`lifecycle_model.py` is a test oracle, not package code.  The source-contract
tests additionally inspect the installed production scripts so the suite
cannot pass only because the model behaves correctly.  When a POSIX shell is
available, `test_sync_integration.py` relocates the real sync script into a
temporary fake OpenWrt root and runs it against the mock UCI CLI in `mocks/`.
