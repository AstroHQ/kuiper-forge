defmodule KuiperCoordinator.AgentGrpc.AgentStreamTest do
  use KuiperCoordinator.AgentGrpcCase
  @moduletag :capture_log
  @moduletag grpc_settings: [first_status_timeout_ms: 300, ping_interval_ms: 60_000]

  alias Cirunner.Agent.{
    AgentMessage,
    CommandAck,
    CoordinatorMessage,
    CreateRunnerCommand,
    LogBatch,
    LogRecord,
    Pong
  }

  alias Cirunner.Agent.AgentService.Stub
  alias KuiperCoordinator.{AgentRegistry, Agents}

  defp take_reply(replies) do
    case Enum.take(replies, 1) do
      [{:ok, %CoordinatorMessage{} = msg}] -> {:ok, msg}
      [{:error, err}] -> {:error, err}
      [] -> :eof
    end
  end

  defp connected!(ctx) do
    {resp, files} = register_agent(ctx)
    {:ok, ch} = connect(ctx, client: files)
    {:ok, ch, resp.agent_id}
  end

  # A1: client cert is optional at TLS but required for AgentService
  test "AgentStream without a client cert is rejected with Unauthenticated", ctx do
    {:ok, ch} = connect(ctx)
    {_stream, result} = open_stream(ch, [status_msg()])
    assert {:error, %GRPC.RPCError{status: 16, message: msg}} = result
    assert msg =~ "mTLS required"
  end

  test "UploadLogs without a client cert is rejected with Unauthenticated", ctx do
    {:ok, ch} = connect(ctx)
    assert {:error, %GRPC.RPCError{status: 16}} = Stub.upload_logs(ch, %LogBatch{})
  end

  test "a client cert from another CA fails the TLS handshake", ctx do
    files = foreign_client_cert(ctx.dir, "agent_tart_evil_00000000")

    # with TLS 1.3 the client may finish its side before the server's alert lands, so the
    # refusal shows up either at connect or on the first call. At connect, the server's close
    # can also race the client's socket setup (e.g. {:options, {:socket_options, :sndbuf,
    # :einval}}), so any transport error counts as refused; what matters is that no call works.
    case connect(ctx, client: files) do
      {:error, %Mint.TransportError{}} ->
        :ok

      {:ok, ch} ->
        assert {:error, %GRPC.RPCError{}} = Stub.upload_logs(ch, %LogBatch{})
    end

    refute AgentRegistry.connected?("agent_tart_evil_00000000")
  end

  # A15 + S2: identity from the cert CN, first status synced, ping sent at once
  test "a registered agent streams: identity from its cert, first status synced, Ping first",
       ctx do
    {:ok, ch, agent_id} = connected!(ctx)
    {_stream, {:ok, replies}} = open_stream(ch, [status_msg(agent_id: "agent_someone_else")])

    assert {:ok, %CoordinatorMessage{payload: {:ping, _}}} = take_reply(replies)
    # the Ping goes out after the stream is registered
    entry = AgentRegistry.lookup(agent_id)
    assert entry.hostname == "mac-mini-1"
    refute AgentRegistry.connected?("agent_someone_else")

    agent = Agents.get_agent(agent_id)
    assert agent.labels == ["self-hosted", "macOS"]
    assert agent.max_vms == 2
    assert agent.agent_version == "0.9.1"
  end

  # S1
  test "a non-status first message is rejected with InvalidArgument", ctx do
    {:ok, ch, agent_id} = connected!(ctx)
    {_stream, result} = open_stream(ch, [%AgentMessage{payload: {:pong, %Pong{}}}])
    assert {:error, %GRPC.RPCError{status: 3}} = first_result(result)
    refute AgentRegistry.connected?(agent_id)
  end

  test "an agent that sends nothing is dropped with DeadlineExceeded", ctx do
    {:ok, ch, agent_id} = connected!(ctx)
    {_stream, result} = open_stream(ch, [])
    assert {:error, %GRPC.RPCError{status: 4}} = first_result(result)
    refute AgentRegistry.connected?(agent_id)
  end

  # A15: revoked agent's next stream open
  test "a revoked agent's next AgentStream gets Unauthenticated", ctx do
    {resp, files} = register_agent(ctx)
    Agents.revoke_agent(resp.agent_id)
    {:ok, ch} = connect(ctx, client: files)
    {_stream, result} = open_stream(ch, [status_msg()])
    assert {:error, %GRPC.RPCError{status: 16, message: msg}} = first_result(result)
    assert msg =~ "revoked"
    # its own connection: the Mint client may drop the first one after the stream's error
    {:ok, ch2} = connect(ctx, client: files)
    assert {:error, %GRPC.RPCError{status: 16}} = Stub.upload_logs(ch2, %LogBatch{})
  end

  # D6: revoke closes the live stream now
  test "revoking an agent closes its live AgentStream", ctx do
    {:ok, ch, agent_id} = connected!(ctx)
    {_stream, {:ok, replies}} = open_stream(ch, [status_msg()])
    assert {:ok, %CoordinatorMessage{payload: {:ping, _}}} = take_reply(replies)
    %{pid: pid} = AgentRegistry.lookup(agent_id)
    ref = Process.monitor(pid)

    assert :ok = Agents.revoke_agent(agent_id)
    assert_receive {:DOWN, ^ref, :process, ^pid, _}, 2_000
    assert {:error, %GRPC.RPCError{status: 16}} = take_reply(replies)
    refute AgentRegistry.connected?(agent_id)
  end

  # A14
  test "a status sync after a revoke leaves the agent revoked", ctx do
    {resp, _} = register_agent(ctx)
    :ok = Agents.revoke_agent(resp.agent_id)
    assert Agents.sync_metadata(resp.agent_id, ["x"], 4, "1.0") == 1
    refute Agents.valid_agent?(resp.agent_id)
    assert Agents.get_agent(resp.agent_id).revoked_at
  end

  # S8: command round trip keyed by command_id
  test "send_command delivers a CreateRunner and returns the matching Ack", ctx do
    {:ok, ch, agent_id} = connected!(ctx)
    {stream, {:ok, replies}} = open_stream(ch, [status_msg()])
    assert {:ok, %CoordinatorMessage{payload: {:ping, _}}} = take_reply(replies)
    assert AgentRegistry.connected?(agent_id)

    cmd = %CreateRunnerCommand{command_id: "cmd-1", vm_name: "runner-1", labels: ["self-hosted"]}

    task =
      Task.async(fn ->
        AgentRegistry.send_command(agent_id, "cmd-1", {:create_runner, cmd}, 2_000)
      end)

    assert {:ok, %CoordinatorMessage{payload: {:create_runner, %{command_id: "cmd-1"}}}} =
             take_reply(replies)

    # a stray ack for another command is dropped
    GRPC.Stub.send_request(stream, %AgentMessage{
      payload: {:ack, %CommandAck{command_id: "other"}}
    })

    GRPC.Stub.send_request(
      stream,
      %AgentMessage{payload: {:ack, %CommandAck{command_id: "cmd-1", accepted: true}}}
    )

    assert {:ok, %CommandAck{command_id: "cmd-1", accepted: true}} = Task.await(task)
  end

  test "send_command times out when the agent never acks", ctx do
    {:ok, ch, agent_id} = connected!(ctx)
    {_stream, {:ok, replies}} = open_stream(ch, [status_msg()])
    assert {:ok, _ping} = take_reply(replies)
    assert AgentRegistry.connected?(agent_id)

    assert {:error, :timeout} =
             AgentRegistry.send_command(
               agent_id,
               "c2",
               {:create_runner, %CreateRunnerCommand{command_id: "c2"}},
               200
             )

    assert {:error, :not_connected} =
             AgentRegistry.send_command(
               "agent_nobody",
               "c3",
               {:ping, %Cirunner.Agent.Ping{}},
               200
             )
  end

  # L1, L2
  test "UploadLogs stores the batch with a dropped-lines marker", ctx do
    {:ok, ch, agent_id} = connected!(ctx)

    batch = %LogBatch{
      dropped: 3,
      records: [
        %LogRecord{
          timestamp_micros: 1_000,
          level: "warn",
          target: "agent",
          message: "one",
          seq: 1
        },
        %LogRecord{
          timestamp_micros: 2_000,
          level: "LOUD",
          target: "agent",
          message: "two",
          seq: 2
        }
      ]
    }

    assert {:ok, %Cirunner.Agent.UploadLogsResponse{}} = Stub.upload_logs(ch, batch)

    assert [
             %{ts: 999, level: 2, message: "agent dropped 3 log line(s)" <> _},
             %{ts: 1_000, level: 2, message: "one", seq: 1},
             %{ts: 2_000, level: 3, message: "two"}
           ] = Agents.log_lines(agent_id)
  end

  test "UploadLogs rejects a batch over 2000 records with InvalidArgument", ctx do
    {:ok, ch, agent_id} = connected!(ctx)
    records = for i <- 1..2001, do: %LogRecord{timestamp_micros: i, level: "INFO", message: "x"}

    assert {:error, %GRPC.RPCError{status: 3}} = Stub.upload_logs(ch, %LogBatch{records: records})
    assert Agents.log_lines(agent_id) == []

    assert {:ok, _} = Stub.upload_logs(ch, %LogBatch{records: Enum.take(records, 2000)})
    assert length(Agents.log_lines(agent_id)) == 2000
  end

  defp first_result({:ok, replies}), do: take_reply(replies)
  defp first_result(other), do: other

  # a revoke while the stream waits for its first status found nothing to close
  @tag grpc_settings: [first_status_timeout_ms: 10_000, ping_interval_ms: 60_000]
  test "an agent revoked during the first-status wait can't register its stream", ctx do
    {:ok, ch, agent_id} = connected!(ctx)
    me = self()
    handler = {__MODULE__, make_ref()}

    :telemetry.attach(
      handler,
      [:kuiper_coordinator, :agent_stream, :authenticated],
      fn _event, _measurements, %{agent_id: id}, _ -> send(me, {:authenticated, id}) end,
      nil
    )

    on_exit(fn -> :telemetry.detach(handler) end)
    stream = Stub.agent_stream(ch)

    # the handler is past authentication and waiting for the first status
    assert_receive {:authenticated, ^agent_id}, 5_000

    :ok = Agents.revoke_agent(agent_id)
    GRPC.Stub.send_request(stream, status_msg())

    assert {:error, %GRPC.RPCError{status: 16}} = GRPC.Stub.recv(stream, timeout: 5_000)
    refute AgentRegistry.connected?(agent_id)
  end

  test "a second concurrent UploadLogs from one agent is turned away before touching the DB",
       ctx do
    {resp, files} = register_agent(ctx)
    {:ok, ch} = connect(ctx, client: files)
    batch = %LogBatch{records: [%Cirunner.Agent.LogRecord{timestamp_micros: 1, message: "x"}]}

    # this test process holds the agent's upload slot, as an in-flight upload would
    {:ok, _} = Registry.register(KuiperCoordinator.AgentGrpc.UploadSlots, resp.agent_id, nil)

    assert {:error, %GRPC.RPCError{status: 14, message: msg}} = Stub.upload_logs(ch, batch)
    assert msg =~ "already in progress"
    assert Agents.log_lines(resp.agent_id) == []

    Registry.unregister(KuiperCoordinator.AgentGrpc.UploadSlots, resp.agent_id)
    assert {:ok, _} = Stub.upload_logs(ch, batch)
    assert [%{message: "x"}] = Agents.log_lines(resp.agent_id)
  end
end
