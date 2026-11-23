defmodule KuiperCoordinator.AgentsTokenLogTest do
  @moduledoc """
  Registration tokens are bearer secrets (D65): Ecto's query log, even at debug, must never carry
  one. Creating, listing, deleting and consuming a token all go through queries with the token as
  a parameter; none of them may print it.
  """
  use KuiperCoordinator.DataCase, async: false
  import ExUnit.CaptureLog
  require Logger

  alias KuiperCoordinator.{Agents, CA}

  setup do
    {ca_cert, ca_key} = KuiperCoordinator.AgentGrpcCase.fixture_ca()
    CA.install!(ca_cert, ca_key)
    level = Logger.level()
    Logger.configure(level: :debug)
    on_exit(fn -> Logger.configure(level: level) end)
    :ok
  end

  test "debug logs of create, list, delete and register never contain the token" do
    log =
      capture_log([level: :debug], fn ->
        {:ok, t1} = Agents.create_registration_token(60)
        _ = Agents.list_registration_tokens()
        assert Agents.delete_registration_token(t1.token)
        {:ok, t2} = Agents.create_registration_token(60)
        assert {:ok, _} = Agents.register(t2.token, "host1", "tart")
        # a token that's gone is still not echoed
        assert {:error, :unauthenticated, _} = Agents.register(t2.token, "host1", "tart")
        send(self(), {:tokens, [t1.token, t2.token]})
      end)

    assert_received {:tokens, tokens}

    # the capture really holds debug query logs (the agent insert is still logged)
    assert log =~ ~s(INSERT INTO "agents")

    for token <- tokens do
      refute log =~ token
      refute log =~ String.slice(token, 4, 32)
    end
  end
end
