defmodule KuiperCoordinator.Fleet.LoaderTest do
  # D39: the fleet runs from the stored forges and reloads on change, without a restart.
  use KuiperCoordinator.DataCase, async: false

  alias KuiperCoordinator.Forges
  alias KuiperCoordinator.Fleet.Loader

  defp pem do
    key = :public_key.generate_key({:rsa, 2048, 65_537})
    :public_key.pem_encode([:public_key.pem_entry_encode(:RSAPrivateKey, key)])
  end

  setup do
    saved = Application.get_env(:kuiper_coordinator, :webhook)
    on_exit(fn -> Application.put_env(:kuiper_coordinator, :webhook, saved || []) end)

    # no real forge is called: the manager's startup scan (webhook mode) gets empty lists
    Req.Test.set_req_test_to_shared()
    stub = {__MODULE__, make_ref()}
    Req.Test.stub(stub, fn conn -> Req.Test.json(conn, []) end)

    loader =
      start_supervised!(
        {Loader,
         name: nil_name(),
         manager: [
           name: nil,
           tick_ms: 60_000,
           async: false,
           req_options: [plug: {Req.Test, stub}, retry: false]
         ]}
      )

    %{loader: loader}
  end

  defp nil_name, do: :"loader_#{System.unique_integer([:positive])}"

  defp running_pid(loader) do
    assert {:running, _mode, pid} = Loader.status(loader)
    pid
  end

  test "no forges: no fleet", %{loader: loader} do
    assert Loader.status(loader) == :off
  end

  defp github(extra \\ %{}),
    do: Map.merge(%{name: "github", type: :github, app_id: 42, private_key: pem()}, extra)

  test "a stored forge starts the fleet; a change restarts it on the new config; delete stops it",
       %{loader: loader} do
    {:ok, forge} =
      Forges.create_forge(github(%{mode: :fixed_capacity, runner_scope: "acme"}))

    assert {:running, %{"github" => :fixed_capacity}, pid1} = Loader.status(loader)
    assert Process.alive?(pid1)
    assert :sys.get_state(pid1).ctx.scope == {:org, "acme"}

    {:ok, forge} = Forges.update_forge(forge, %{runner_scope: "acme/repo"})
    pid2 = running_pid(loader)
    refute pid2 == pid1
    refute Process.alive?(pid1)
    assert :sys.get_state(pid2).ctx.scope == {:repo, "acme", "repo"}

    {:ok, _} = Forges.delete_forge(forge)
    assert Loader.status(loader) == :off
    refute Process.alive?(pid2)
  end

  test "an unchanged config leaves the running fleet alone", %{loader: loader} do
    {:ok, _} = Forges.create_forge(github())
    pid = running_pid(loader)
    {:ok, _} = Forges.update_settings(%{idle_runner_timeout_s: 600})
    assert running_pid(loader) == pid
  end

  test "each forge runs in its own mode, side by side", %{loader: loader} do
    # the startup pass reconciles the fixed-capacity forge's pools; there are none here
    Mox.stub(KuiperCoordinator.AgentDispatchMock, :pool_definitions, fn -> [] end)

    {:ok, _} = Forges.create_forge(github(%{mode: :fixed_capacity, runner_scope: "acme"}))

    {:ok, _} =
      Forges.create_forge(
        github(%{name: "hooked", app_id: 43, mode: :webhook, webhook_secret: "hook"})
      )

    {:ok, _} = Forges.create_forge(github(%{name: "polled", app_id: 44, poll_interval_s: 30}))

    assert {:running, modes, pid} = Loader.status(loader)
    assert modes == %{"github" => :fixed_capacity, "hooked" => :webhook, "polled" => :polling}

    state = :sys.get_state(pid)
    assert state.ctx.forge_name == "github"

    assert %{"hooked" => %{kind: :webhook}, "polled" => %{kind: :polling, every_ms: 30_000}} =
             state.scans

    refute is_map_key(state.scans, "github")
  end

  test "forges that don't fit together turn the fleet off with the reason", %{loader: loader} do
    {:ok, _} = Forges.create_forge(github(%{mode: :fixed_capacity, runner_scope: "acme"}))
    pid = running_pid(loader)

    # saving refuses a second fixed-capacity forge, so this one goes in behind its back
    KuiperCoordinator.Repo.insert!(
      struct(Forges.Forge, github(%{name: "other", mode: :fixed_capacity, runner_scope: "acme"}))
    )

    {:ok, _} = Forges.update_settings(%{idle_runner_timeout_s: 300})
    assert {:error, msg} = Loader.status(loader)
    assert msg =~ "only one forge can use fixed capacity"
    refute Process.alive?(pid)
    assert Application.get_env(:kuiper_coordinator, :webhook) == []
  end

  test "a forge in webhook mode is published as a webhook source for the plugs",
       %{loader: loader} do
    {:ok, _} = Forges.create_forge(github(%{mode: :webhook, webhook_secret: "hook"}))
    {:ok, _} = Forges.create_forge(github(%{name: "polled", app_id: 43}))

    assert {:running, %{"github" => :webhook}, _} = Loader.status(loader)
    webhook = Application.get_env(:kuiper_coordinator, :webhook)
    assert webhook[:path] == "/webhook"
    assert webhook[:sources] == [%{forge: "github", secret: "hook", source: :github}]
  end
end
