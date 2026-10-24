# Service startup ordering

`fcp` must start before `fcptemp`: `fcptemp` sends automatic PWM commands through the FCP Unix-socket API. The dependency belongs in systemd ordering, not the opposite way round.

A unit with `Type=simple` is considered started once its process has been launched. That does not guarantee `/run/fcp/control.sock` already exists. `fcp.service` therefore uses `ExecStartPost` to wait up to 10 seconds for the socket. Systemd does not consider FCP startup complete until the post-start check succeeds, so `After=fcp.service` delays `fcptemp` until readiness.

If the socket fails to appear, FCP startup fails and systemd retries it. `fcptemp` itself also keeps running its control loop and retries FCP API calls each polling cycle; it does not need to exit just because FCP is temporarily unavailable.

To apply the unit files and restart the services:

```sh
./scripts/install-fcptemp-service.sh
```

Inspect startup logs:

```sh
journalctl -b -u fcp.service -u fcptemp.service --no-pager
```
