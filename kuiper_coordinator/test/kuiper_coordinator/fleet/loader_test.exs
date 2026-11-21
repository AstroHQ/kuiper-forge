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

  test "a stored forge starts the fleet; a change restarts it on the new config; delete stops it",
       %{loader: loader} do
    {:ok, _} = Forges.update_settings(%{runner_scope: "acme"})

    {:ok, forge} =
      Forges.create_forge(%{name: "github", type: :github, app_id: 42, private_key: pem()})

    assert {:running, :fixed_capacity, pid1} = Loader.status(loader)
    assert Process.alive?(pid1)
    assert :sys.get_state(pid1).ctx.scope == {:org, "acme"}

    {:ok, _} = Forges.update_settings(%{runner_scope: "acme/repo"})
    pid2 = running_pid(loader)
    refute pid2 == pid1
    refute Process.alive?(pid1)
    assert :sys.get_state(pid2).ctx.scope == {:repo, "acme", "repo"}

    {:ok, _} = Forges.delete_forge(forge)
    assert Loader.status(loader) == :off
    refute Process.alive?(pid2)
  end

  test "an unchanged config leaves the running fleet alone", %{loader: loader} do
    {:ok, _} = Forges.update_settings(%{runner_scope: "acme"})

    {:ok, _} =
      Forges.create_forge(%{name: "github", type: :github, app_id: 42, private_key: pem()})

    pid = running_pid(loader)
    {:ok, _} = Forges.update_settings(%{runner_scope: "acme"})
    assert running_pid(loader) == pid
  end

  test "forges and settings that don't fit together turn the fleet off with the reason",
       %{loader: loader} do
    {:ok, _} = Forges.update_settings(%{runner_scope: "acme"})

    {:ok, _} =
      Forges.create_forge(%{name: "github", type: :github, app_id: 42, private_key: pem()})

    pid = running_pid(loader)

    {:ok, _} = Forges.update_settings(%{mode: :webhook})
    assert {:error, msg} = Loader.status(loader)
    assert msg =~ "webhook secret"
    refute Process.alive?(pid)
    assert Application.get_env(:kuiper_coordinator, :webhook) == []
  end

  test "webhook mode publishes the webhook config for the plugs", %{loader: loader} do
    {:ok, _} = Forges.update_settings(%{mode: :webhook})

    {:ok, _} =
      Forges.create_forge(%{
        name: "github",
        type: :github,
        app_id: 42,
        private_key: pem(),
        webhook_secret: "hook"
      })

    assert {:running, :webhook, _} = Loader.status(loader)
    webhook = Application.get_env(:kuiper_coordinator, :webhook)
    assert webhook[:secret] == "hook"
    assert webhook[:source] == :github
  end
end
