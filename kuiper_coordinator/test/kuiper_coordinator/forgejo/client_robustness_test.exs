defmodule KuiperCoordinator.Forgejo.ClientRobustnessTest do
  # Nye's Forgejo read (Nov 21), items 4 and 5.
  use ExUnit.Case, async: true

  alias KuiperCoordinator.Forgejo.Client

  defp client(stub, extra \\ []) do
    {:ok, c} =
      Client.new(
        [
          url: "http://forgejo.test",
          token: "secret",
          scopes: ["acme"],
          req_options: [plug: {Req.Test, stub}, retry: false]
        ] ++ extra
      )

    c
  end

  test "a 2xx that isn't JSON (a proxy's HTML page) is an error, not a crash (Rust 'Failed to parse')" do
    stub = {__MODULE__, make_ref()}

    Req.Test.stub(stub, fn conn ->
      conn
      |> Plug.Conn.put_resp_content_type("text/html")
      |> Plug.Conn.send_resp(200, "<html><body>Sign in to the proxy</body></html>")
    end)

    assert {:error, msg} = Client.list_runners(client(stub), {:org, "acme"})
    assert msg =~ "Failed to parse"
  end

  test "the repo-name cache outlives the process that built the client" do
    stub = {__MODULE__, make_ref()}
    test_pid = self()

    Req.Test.stub(stub, fn conn ->
      case conn.path_info do
        ["api", "v1", "repositories", "7"] ->
          send(test_pid, :repo_lookup)
          Req.Test.json(conn, %{"full_name" => "acme/app"})

        _ ->
          Req.Test.json(conn, [
            %{"id" => 1, "handle" => "h1", "repo_id" => 7, "labels" => [], "status" => "waiting"}
          ])
      end
    end)

    # built in a short-lived process (a config load, a Task) that then dies
    builder =
      spawn(fn ->
        send(test_pid, {:client, client(stub)})
        Process.sleep(:infinity)
      end)

    c = receive(do: ({:client, c} -> c))
    ref = Process.monitor(builder)
    Process.exit(builder, :kill)
    assert_receive {:DOWN, ^ref, _, _, _}
    Process.sleep(10)
    assert {:ok, [_ | _]} = Client.list_queued_jobs(c)
    assert {:ok, [_ | _]} = Client.list_queued_jobs(c)
    assert_received :repo_lookup
    refute_received :repo_lookup
  end

  test "a shared cache can be passed in (supervised by the fleet)" do
    {:ok, cache} = Agent.start_link(fn -> %{} end)
    stub = {__MODULE__, make_ref()}
    Req.Test.stub(stub, fn conn -> Req.Test.json(conn, []) end)
    assert %Client{repo_names: ^cache} = client(stub, cache: cache)
  end
end
