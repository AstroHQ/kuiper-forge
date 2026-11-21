defmodule KuiperCoordinator.AgentInputD25Test do
  # D25: UploadLogs retries don't duplicate lines, and agent-supplied text is escaped in logs.
  # async: false, the log test raises the global Logger level
  use KuiperCoordinator.DataCase, async: false

  import ExUnit.CaptureLog
  require Logger

  alias Cirunner.Agent.{LogRecord, RegisterRequest}
  alias KuiperCoordinator.Agents

  test "re-sending the same log batch (an agent retry) stores each line once" do
    agent_id = "agent_tart_h_retry"

    records = [
      %LogRecord{timestamp_micros: 10, seq: 1, level: "INFO", message: "one"},
      %LogRecord{timestamp_micros: 10, seq: 2, level: "INFO", message: "two"}
    ]

    assert :ok = Agents.insert_log_batch(agent_id, records, 3)
    assert :ok = Agents.insert_log_batch(agent_id, records, 3)

    assert ["agent dropped 3 log line(s) because its upload buffer was full", "one", "two"] ==
             Enum.map(Agents.log_lines(agent_id), & &1.message)
  end

  test "hostname and agent_type are escaped in log lines, so an agent can't forge lines" do
    req = %RegisterRequest{
      registration_token: "reg_nope",
      hostname: "evil\n12:00:00.000 [info] agent registered agent_id=forged",
      agent_type: "tart\r\nx"
    }

    level = Logger.level()
    Logger.configure(level: :info)
    on_exit(fn -> Logger.configure(level: level) end)

    log =
      capture_log([level: :info], fn ->
        try do
          KuiperCoordinator.AgentGrpc.RegistrationServer.register(req, nil)
        rescue
          GRPC.RPCError -> :ok
        end
      end)

    refute log =~ "\n12:00:00.000 [info] agent registered"
    refute log =~ "\r\n"
    assert log =~ ~s(hostname="evil\\n12:00)
  end
end
