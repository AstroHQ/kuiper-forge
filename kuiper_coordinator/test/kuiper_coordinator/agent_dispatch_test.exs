defmodule KuiperCoordinator.AgentDispatchTest do
  use ExUnit.Case, async: true
  import Mox

  alias KuiperCoordinator.{AgentDispatch, AgentDispatchMock}
  alias Cirunner.Agent.{CreateRunnerCommand, CreateRunnerResult}

  setup :verify_on_exit!

  test "provisioning reaches the configured agent dispatcher, slots keyed by runner name" do
    expect(AgentDispatchMock, :reserve_slot, fn ["self-hosted"],
                                                "pve01-abcd1234",
                                                [avoid: ["a2"]] ->
      {:ok, "a1"}
    end)

    expect(AgentDispatchMock, :dispatch_create, fn "a1",
                                                   %CreateRunnerCommand{vm_name: "pve01-abcd1234"} ->
      :ok
    end)

    expect(AgentDispatchMock, :release_slot, fn "a1", "pve01-abcd1234" -> :ok end)
    expect(AgentDispatchMock, :dispatch_destroy, fn "a1", "pve01-abcd1234" -> :ok end)

    assert {:ok, "a1"} =
             AgentDispatch.reserve_slot(["self-hosted"], "pve01-abcd1234", avoid: ["a2"])

    assert :ok =
             AgentDispatch.dispatch_create("a1", %CreateRunnerCommand{vm_name: "pve01-abcd1234"})

    assert :ok = AgentDispatch.release_slot("a1", "pve01-abcd1234")
    assert :ok = AgentDispatch.dispatch_destroy("a1", "pve01-abcd1234")
  end

  test "a legacy agent's full result and a rejected ack pass through" do
    expect(AgentDispatchMock, :dispatch_create, 2, fn
      "legacy", _ -> {:ok, %CreateRunnerResult{vm_id: "101"}}
      "busy", _ -> {:error, {:rejected, "at capacity"}}
    end)

    assert {:ok, %CreateRunnerResult{vm_id: "101"}} =
             AgentDispatch.dispatch_create("legacy", %CreateRunnerCommand{})

    assert {:error, {:rejected, "at capacity"}} =
             AgentDispatch.dispatch_create("busy", %CreateRunnerCommand{})
  end

  test "without an agent registry nothing is available" do
    assert {:error, :no_available_agent} =
             KuiperCoordinator.AgentDispatch.None.reserve_slot([], "r1", [])

    assert {:error, :not_connected} =
             KuiperCoordinator.AgentDispatch.None.dispatch_create("a1", %CreateRunnerCommand{})
  end
end
