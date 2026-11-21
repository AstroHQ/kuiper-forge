defmodule KuiperCoordinator.ProtoTest do
  # The generated modules must speak the same wire format as the Rust agents.
  # The golden bytes below come from protoc itself (the canonical encoder), e.g.
  #
  #     printf 'registration_token: "tok-123" ...' |
  #       protoc --encode=cirunner.agent.RegisterRequest \
  #         -I ../kuiper-agent-proto/proto agent.proto | od -An -tx1
  #
  # so a wrong field number or type in the generated code fails here.
  use ExUnit.Case, async: true

  alias Cirunner.Agent.{
    AgentMessage,
    AgentStatus,
    CapacityLimit,
    CoordinatorMessage,
    CreateRunnerCommand,
    LabelSet,
    RegisterRequest,
    VmInfo
  }

  @register_request_hex "0a07746f6b2d313233120a6d61632d6d696e692d311a0474617274"

  @agent_status_hex "0a8401080110031a230a04766d2d31120872756e6e65722d311a0772756e6e696e67220831302e302e302e352a0a6d61632d6d696e692d3132047461727440044a280a0b73656c662d686f737465640a056d61636f7312056d61636f7318022207736571756f696128015205302e392e305a0b0a056d61636f73100220016207666f7267656a6f"

  defp status do
    %AgentStatus{
      active_vms: 1,
      available_slots: 3,
      vms: [
        %VmInfo{vm_id: "vm-1", name: "runner-1", state: "running", ip_address: "10.0.0.5"}
      ],
      hostname: "mac-mini-1",
      agent_type: "tart",
      max_vms: 4,
      label_sets: [
        %LabelSet{
          labels: ["self-hosted", "macos"],
          limits: ["macos"],
          pool_size: 2,
          id: "sequoia",
          is_default: true
        }
      ],
      agent_version: "0.9.0",
      limits: [%CapacityLimit{name: "macos", max: 2, active: 1}],
      runner_kinds: ["forgejo"]
    }
  end

  test "RegisterRequest decodes protoc's bytes and encodes back to them" do
    bytes = Base.decode16!(@register_request_hex, case: :lower)

    request = RegisterRequest.decode(bytes)

    assert request == %RegisterRequest{
             registration_token: "tok-123",
             hostname: "mac-mini-1",
             agent_type: "tart"
           }

    assert RegisterRequest.encode(request) == bytes
  end

  test "AgentMessage with a status payload round-trips through the wire format" do
    bytes = Base.decode16!(@agent_status_hex, case: :lower)
    message = %AgentMessage{payload: {:status, status()}}

    assert AgentMessage.decode(bytes) == message
    assert message |> AgentMessage.encode() |> AgentMessage.decode() == message
  end

  test "unset optional pool_size stays distinct from zero" do
    unset = %LabelSet{labels: ["linux"]} |> LabelSet.encode() |> LabelSet.decode()
    zero = %LabelSet{labels: ["linux"], pool_size: 0} |> LabelSet.encode() |> LabelSet.decode()

    assert unset.pool_size == nil
    assert zero.pool_size == 0
  end

  test "CoordinatorMessage oneof round-trips" do
    message = %CoordinatorMessage{
      payload: {:create_runner, %CreateRunnerCommand{command_id: "cmd-1", vm_name: "r-1"}}
    }

    assert message |> CoordinatorMessage.encode() |> CoordinatorMessage.decode() == message
  end

  test "generated gRPC services expose the proto's rpcs" do
    assert Cirunner.Agent.RegistrationService.Service.__meta__(:name) ==
             "cirunner.agent.RegistrationService"

    rpcs = Cirunner.Agent.AgentService.Service.__rpc_calls__() |> Enum.map(&elem(&1, 0))
    assert :AgentStream in rpcs
    assert :UploadLogs in rpcs

    assert Cirunner.Management.ManagementService.Service.__meta__(:name) ==
             "cirunner.management.ManagementService"
  end
end
