defmodule KuiperCoordinator.AgentGrpc.RustAgentTest do
  @moduledoc """
  A real kuiper-agent-lib agent (test/rust_agent_probe) against the Phoenix agent port with the
  CA made by the Rust `kuiper-forge ca init`. Excluded by default; run with the probe built:

      cargo build --manifest-path test/rust_agent_probe/Cargo.toml
      KUIPER_AGENT_PROBE=test/rust_agent_probe/target/debug/kuiper-agent-probe mix test --only rust_agent
  """
  use KuiperCoordinator.AgentGrpcCase
  @moduletag :rust_agent
  @moduletag :capture_log
  @moduletag timeout: 120_000

  alias Cirunner.Agent.CreateRunnerCommand
  alias KuiperCoordinator.{AgentRegistry, Agents}

  test "a real agent registers from a bundle, streams, acks a command and uploads logs", ctx do
    probe =
      System.get_env("KUIPER_AGENT_PROBE") || flunk("set KUIPER_AGENT_PROBE to the probe binary")

    {:ok, token} = Agents.create_registration_token()
    bundle = Agents.registration_bundle(token, "https://localhost:#{ctx.port}")
    cert_dir = Path.join(ctx.dir, "agent-certs")
    Phoenix.PubSub.subscribe(KuiperCoordinator.PubSub, AgentRegistry.topic())

    task =
      Task.async(fn ->
        System.cmd(Path.expand(probe), [bundle, cert_dir], stderr_to_stdout: true)
      end)

    assert_receive {:agent_connected, agent_id, %{hostname: "probe-host"}}, 60_000
    assert agent_id =~ ~r/\Aagent_tart_/

    cmd = %CreateRunnerCommand{
      command_id: "cmd-probe-1",
      vm_name: "runner-probe-1",
      labels: ["self-hosted"]
    }

    assert {:ok, %{command_id: "cmd-probe-1", accepted: true}} =
             AgentRegistry.send_command(agent_id, "cmd-probe-1", {:create_runner, cmd}, 15_000)

    assert_receive {:agent_event, ^agent_id,
                    {:runner_event, %{runner_name: "runner-probe-1", event_type: :STARTED}}},
                   15_000

    {out, status} = Task.await(task, 60_000)
    assert status == 0, out
    assert out =~ "ALL PASS", out

    agent = Agents.get_agent(agent_id)
    assert agent.labels == ["self-hosted", "macOS"]
    assert agent.max_vms == 2
    assert agent.agent_version == "0.0.0-probe"
    assert [%{message: "hello from the probe", target: "probe"}] = Agents.log_lines(agent_id)
  end

  @runtime Path.expand("../../../config/runtime.exs", __DIR__)
  @env ~w(PORT KUIPER_HTTP_PORT KUIPER_AGENT_PORT KUIPER_AGENT_GRPC KUIPER_CA_CERT KUIPER_CA_KEY KUIPER_SERVER_CERT KUIPER_SERVER_KEY)

  # D21-D23: agent gRPC listens on KUIPER_AGENT_PORT through config/runtime.exs, apart from
  # Phoenix's HTTP port (PORT).
  @tag :no_server
  test "a real agent uses the agent port from KUIPER_AGENT_PORT, apart from PORT", ctx do
    probe =
      System.get_env("KUIPER_AGENT_PROBE") || flunk("set KUIPER_AGENT_PROBE to the probe binary")

    saved = Map.new(@env, &{&1, System.get_env(&1)})

    on_exit(fn ->
      Enum.each(saved, fn
        {k, nil} -> System.delete_env(k)
        {k, v} -> System.put_env(k, v)
      end)
    end)

    port = free_port()
    http_port = free_port()
    System.delete_env("KUIPER_HTTP_PORT")
    System.put_env("PORT", Integer.to_string(http_port))
    System.put_env("KUIPER_AGENT_PORT", Integer.to_string(port))
    System.put_env("KUIPER_AGENT_GRPC", "true")
    System.put_env("KUIPER_CA_CERT", ctx.certs.ca_cert)
    System.put_env("KUIPER_CA_KEY", ctx.certs.ca_key)
    System.put_env("KUIPER_SERVER_CERT", ctx.certs.server_cert)
    System.put_env("KUIPER_SERVER_KEY", ctx.certs.server_key)

    config = Config.Reader.read!(@runtime, env: :dev, target: :host)

    assert get_in(config, [:kuiper_coordinator, KuiperCoordinatorWeb.Endpoint, :http, :port]) ==
             http_port

    settings = get_in(config, [:kuiper_coordinator, :agent_grpc]) ++ [ip: {127, 0, 0, 1}]
    assert settings[:enabled] and settings[:port] == port
    Application.put_env(:kuiper_coordinator, :agent_grpc, settings)
    start_supervised!({KuiperCoordinator.AgentGrpc.Supervisor, settings})

    {:ok, token} = Agents.create_registration_token()
    bundle = Agents.registration_bundle(token, "https://localhost:#{port}")
    Phoenix.PubSub.subscribe(KuiperCoordinator.PubSub, AgentRegistry.topic())

    task =
      Task.async(fn ->
        System.cmd(Path.expand(probe), [bundle, Path.join(ctx.dir, "agent-certs")],
          stderr_to_stdout: true
        )
      end)

    assert_receive {:agent_connected, agent_id, %{hostname: "probe-host"}}, 60_000

    cmd = %CreateRunnerCommand{command_id: "cmd-1p", vm_name: "runner-1p", labels: ["x"]}

    assert {:ok, %{command_id: "cmd-1p", accepted: true}} =
             AgentRegistry.send_command(agent_id, "cmd-1p", {:create_runner, cmd}, 15_000)

    {out, status} = Task.await(task, 60_000)
    assert status == 0, out
    assert out =~ "ALL PASS", out
  end
end
