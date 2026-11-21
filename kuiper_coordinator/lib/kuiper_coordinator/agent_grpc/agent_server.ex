defmodule KuiperCoordinator.AgentGrpc.AgentServer do
  @moduledoc """
  AgentService: the AgentStream bidi stream and UploadLogs, both behind mTLS.

  AgentStream (parity §1b): the first message must be an AgentStatus within the first-status
  deadline (S1); its labels/max_vms/version are synced (S2); the stream registers in
  `KuiperCoordinator.AgentRegistry`; a Ping goes out at once and then every ping interval (S5).
  A reader process forwards the agent's messages; this handler process owns sending, so commands
  can go out at any time. The registry closes the stream on revoke (D6), on a newer connection
  for the same agent (D5) and when the agent goes silent (S5).
  """
  use GRPC.Server, service: Cirunner.Agent.AgentService.Service

  alias Cirunner.Agent.{AgentMessage, AgentStatus, CoordinatorMessage, Ping, UploadLogsResponse}
  alias GRPC.{RPCError, Status}
  alias KuiperCoordinator.{AgentGrpc, AgentRegistry, Agents}
  alias KuiperCoordinator.AgentGrpc.Auth
  require Logger

  def upload_logs(batch, stream) do
    # identity from the cert first; the slot is taken before the DB-backed validity check, so
    # concurrent calls from one agent never reach the pool
    agent_id = Auth.agent_id!(stream)
    max = Agents.max_log_batch()

    if length(batch.records) > max do
      raise RPCError,
        status: Status.invalid_argument(),
        message: "Too many log records in one batch (max #{max})"
    end

    with_upload_slot(agent_id, fn ->
      Auth.valid_agent!(agent_id)

      try do
        Agents.insert_log_batch(agent_id, batch.records, batch.dropped)
      rescue
        e ->
          # the agent requeues on a non-refusal error, so a db hiccup only delays the lines
          Logger.warning("failed to store logs for #{agent_id}: #{Exception.message(e)}")
          raise RPCError, status: Status.unavailable(), message: "Failed to store logs"
      end
    end)

    %UploadLogsResponse{}
  end

  # One upload per agent at a time, taken before any DB connection is checked out, so one agent
  # can't tie up the pool. The Registry entry goes with this process however it ends.
  defp with_upload_slot(agent_id, fun) do
    case Registry.register(KuiperCoordinator.AgentGrpc.UploadSlots, agent_id, nil) do
      {:ok, _} ->
        try do
          fun.()
        after
          Registry.unregister(KuiperCoordinator.AgentGrpc.UploadSlots, agent_id)
        end

      # Unavailable, not ResourceExhausted: the agent drops a refused batch but requeues this
      {:error, {:already_registered, _}} ->
        raise RPCError,
          status: Status.unavailable(),
          message: "An upload for this agent is already in progress"
    end
  end

  def agent_stream(req_enum, stream) do
    agent_id = Auth.authenticate!(stream)

    :telemetry.execute([:kuiper_coordinator, :agent_stream, :authenticated], %{}, %{
      agent_id: agent_id
    })

    me = self()

    # Demand-first: the reader forwards one message, then waits until it's handled, so a fast
    # agent is held back by HTTP/2 flow control instead of filling this mailbox (and a revoke
    # never queues behind a backlog).
    reader =
      spawn_link(fn ->
        Enum.each(req_enum, fn msg ->
          send(me, {:agent_msg, msg})

          receive do
            :more -> :ok
          end
        end)

        send(me, :agent_eof)
      end)

    Process.put(:agent_reader, reader)

    status = await_first_status!()
    agent_type = parse_type!(status.agent_type)

    Agents.sync_metadata(agent_id, status.labels, status.max_vms, status.agent_version)

    :ok =
      AgentRegistry.register(agent_id, %{
        hostname: status.hostname,
        agent_type: agent_type,
        status: status,
        vm_names: Enum.map(status.vms, & &1.name)
      })

    # A revoke during the first-status wait found no stream to close. Checking again after
    # registering closes that gap: a later revoke finds this stream in the registry.
    unless Agents.valid_agent?(agent_id) do
      AgentRegistry.unregister(agent_id)
      raise RPCError, status: Status.unauthenticated(), message: "Agent is revoked or expired"
    end

    Logger.info(
      "agent #{agent_id} connected hostname=#{inspect(status.hostname)} vms=#{length(status.vms)}"
    )

    push(stream, {:ping, %Ping{}})
    Process.send_after(self(), :ping, AgentGrpc.setting(:ping_interval_ms, 30_000))

    try do
      loop(stream, agent_id, %{})
    after
      AgentRegistry.unregister(agent_id)
    end
  end

  defp await_first_status! do
    timeout = AgentGrpc.setting(:first_status_timeout_ms, 30_000)

    receive do
      {:agent_msg, %AgentMessage{payload: {:status, %AgentStatus{} = status}}} ->
        demand()
        status

      {:agent_msg, _} ->
        raise RPCError,
          status: Status.invalid_argument(),
          message: "First message must be a status message"

      :agent_eof ->
        raise RPCError, status: Status.invalid_argument(), message: "No initial message received"
    after
      timeout ->
        raise RPCError,
          status: Status.deadline_exceeded(),
          message: "Timed out waiting for initial status message"
    end
  end

  defp parse_type!(type) do
    case Agents.parse_agent_type(type) do
      {:ok, t} -> t
      {:error, msg} -> raise RPCError, status: Status.invalid_argument(), message: msg
    end
  end

  defp loop(stream, agent_id, pending) do
    receive do
      {:agent_msg, msg} ->
        pending = handle_agent_msg(msg, agent_id, pending)
        demand()
        loop(stream, agent_id, pending)

      :agent_eof ->
        Logger.info("agent #{agent_id} closed its stream")
        :ok

      :ping ->
        push(stream, {:ping, %Ping{}})
        Process.send_after(self(), :ping, AgentGrpc.setting(:ping_interval_ms, 30_000))
        loop(stream, agent_id, pending)

      {:push, payload} ->
        push(stream, payload)
        loop(stream, agent_id, pending)

      {:send_command, caller, ref, command_id, payload} ->
        push(stream, payload)
        loop(stream, agent_id, Map.put(pending, command_id, {caller, ref}))

      {:forget_command, command_id} ->
        loop(stream, agent_id, Map.delete(pending, command_id))

      {:disconnect, :revoked} ->
        Logger.info("closing stream of revoked agent #{agent_id}")
        raise RPCError, status: Status.unauthenticated(), message: "Agent has been revoked"

      {:disconnect, reason} ->
        Logger.info("closing stream of agent #{agent_id}: #{reason}")
        raise RPCError, status: Status.unavailable(), message: "Stream closed: #{reason}"
    end
  end

  defp handle_agent_msg(%AgentMessage{payload: {:status, status}}, agent_id, pending) do
    Agents.sync_metadata(agent_id, status.labels, status.max_vms, status.agent_version)
    AgentRegistry.touch(agent_id, status)
    pending
  end

  defp handle_agent_msg(%AgentMessage{payload: {:pong, _}}, agent_id, pending) do
    AgentRegistry.touch(agent_id)
    pending
  end

  defp handle_agent_msg(%AgentMessage{payload: {:ack, ack}}, _agent_id, pending) do
    case Map.pop(pending, ack.command_id) do
      {{caller, ref}, pending} ->
        send(caller, {ref, {:ack, ack}})
        pending

      # late or unknown ack (S8): dropped
      {nil, pending} ->
        pending
    end
  end

  defp handle_agent_msg(%AgentMessage{payload: {kind, msg}}, agent_id, pending)
       when kind in [:result, :runner_event] do
    AgentRegistry.broadcast({:agent_event, agent_id, {kind, msg}})
    pending
  end

  defp handle_agent_msg(_other, _agent_id, pending), do: pending

  # lets the reader forward the agent's next message
  defp demand, do: send(Process.get(:agent_reader), :more)

  defp push(stream, payload),
    do: GRPC.Server.send_reply(stream, %CoordinatorMessage{payload: payload})
end
