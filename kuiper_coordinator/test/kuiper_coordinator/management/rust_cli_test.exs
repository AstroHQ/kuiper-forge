defmodule KuiperCoordinator.Management.RustCliTest do
  @moduledoc """
  The Rust CLI's management client (test/rust_agent_probe/src/bin/kuiper-management-probe.rs,
  the same connector as `kuiper-forge/src/management.rs` ManagementClient) against the Phoenix
  management socket. Excluded by default; run with the probe built:

      cargo build --manifest-path test/rust_agent_probe/Cargo.toml
      KUIPER_MANAGEMENT_PROBE=test/rust_agent_probe/target/debug/kuiper-management-probe mix test --only rust_agent
  """
  use ExUnit.Case, async: false
  @moduletag :rust_agent
  @moduletag :capture_log
  @moduletag timeout: 120_000

  alias KuiperCoordinator.Agents.{Agent, RegistrationToken}
  alias KuiperCoordinator.Repo

  @fixtures Path.expand("../../fixtures/rust_init_ca", __DIR__)

  setup tags do
    KuiperCoordinator.DataCase.setup_sandbox(Map.put(tags, :async, false))
    dir = Path.join("/tmp", "kcr-#{System.unique_integer([:positive])}")
    on_exit(fn -> File.rm_rf!(dir) end)
    socket = Path.join(dir, "management.sock")

    old = Application.get_env(:kuiper_coordinator, :agent_grpc)

    Application.put_env(:kuiper_coordinator, :agent_grpc,
      ca_cert: Path.join(@fixtures, "ca.crt"),
      ca_key: Path.join(@fixtures, "ca.key"),
      server_trust_mode: "ca"
    )

    on_exit(fn -> Application.put_env(:kuiper_coordinator, :agent_grpc, old || []) end)
    start_supervised!({KuiperCoordinator.Management.Supervisor, socket: socket})
    {:ok, socket: socket}
  end

  test "the Rust CLI client creates, lists and deletes tokens and lists and revokes agents",
       %{socket: socket} do
    probe =
      System.get_env("KUIPER_MANAGEMENT_PROBE") ||
        flunk("set KUIPER_MANAGEMENT_PROBE to the management probe binary")

    agent =
      Repo.insert!(%Agent{
        agent_id: "agent_tart_probe",
        hostname: "probe-host",
        agent_type: "tart",
        labels: ["macos"],
        max_vms: 2,
        serial_number: "01",
        expires_at: DateTime.add(DateTime.utc_now(), 86_400, :second)
      })

    {out, status} =
      System.cmd(Path.expand(probe), [socket, agent.agent_id], stderr_to_stdout: true)

    assert status == 0, out
    assert out =~ "ALL PASS"
    [_, token] = Regex.run(~r/^TOKEN (\S+)$/m, out)
    refute Repo.get(RegistrationToken, token)
    assert Repo.get!(Agent, agent.agent_id).revoked_at
  end
end
