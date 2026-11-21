defmodule KuiperCoordinator.Forgejo.ClientTest do
  # Ported from the tests in kuiper-forge/src/forgejo.rs (parity FJ1-FJ7), against the same fake.
  use ExUnit.Case, async: true

  alias KuiperCoordinator.Forge
  alias KuiperCoordinator.Forgejo.Client

  @base "http://forgejo.test"

  # The fake Forgejo from forgejo.rs's tests. `opts`: :runners, :no_repo_read, :login,
  # :max_page_size. Deletes, registrations and repo lookups are sent to the test process.
  defp fake(opts \\ []) do
    test_pid = self()
    name = {__MODULE__, make_ref()}

    Req.Test.stub(name, fn conn ->
      conn = Plug.Conn.fetch_query_params(conn)
      authed? = Plug.Conn.get_req_header(conn, "authorization") == ["token secret"]
      route(conn, conn.method, conn.path_info, authed?, opts, test_pid)
    end)

    name
  end

  defp json(conn, status, body) do
    conn |> Plug.Conn.put_status(status) |> Req.Test.json(body)
  end

  @org_jobs [
    %{
      "id" => 1,
      "handle" => "h-wait",
      "repo_id" => 7,
      "runs_on" => ["linux"],
      "status" => "waiting",
      "name" => "build"
    },
    %{
      "id" => 2,
      "handle" => "h-run",
      "repo_id" => 7,
      "runs_on" => ["linux"],
      "status" => "running",
      "name" => "test"
    },
    %{
      "id" => 3,
      "handle" => "",
      "repo_id" => 7,
      "runs_on" => ["linux"],
      "status" => "waiting",
      "name" => "old"
    }
  ]
  @solo %{
    "id" => 9,
    "handle" => "h-solo",
    "repo_id" => 8,
    "runs_on" => ["linux"],
    "status" => "waiting",
    "name" => "solo"
  }

  defp route(conn, "GET", ~w(api v1 orgs lodge actions runners jobs), authed?, _o, _p) do
    if authed?, do: json(conn, 200, @org_jobs), else: json(conn, 401, %{})
  end

  defp route(conn, "GET", ~w(api v1 orgs empty actions runners jobs), _, _o, _p),
    do: json(conn, 200, nil)

  defp route(conn, "GET", ~w(api v1 orgs empty actions runners), _, _o, _p),
    do: json(conn, 200, nil)

  defp route(conn, "GET", ~w(api v1 orgs ro actions runners jobs), _, _o, _p),
    do: json(conn, 200, nil)

  defp route(conn, "DELETE", ["api", "v1", "orgs", "ro", "actions", "runners", _], _, _o, _p),
    do:
      json(conn, 403, %{
        "message" => "token does not have at least one of required scope(s): [write:organization]"
      })

  defp route(conn, "GET", ~w(api v1 repos bad repo actions runners jobs), _, _o, _p),
    do: json(conn, 403, %{"message" => "not owner"})

  defp route(conn, "GET", ["api", "v1", "repositories", id], _, o, pid) do
    cond do
      o[:no_repo_read] ->
        json(conn, 403, %{
          "message" => "token does not have at least one of required scope(s): [read:repository]"
        })

      id == "0" ->
        json(conn, 404, %{})

      true ->
        send(pid, {:repo_lookup, id})
        name = if id == "8", do: "kbot/solo", else: "lodge/app"
        json(conn, 200, %{"id" => String.to_integer(id), "full_name" => name})
    end
  end

  defp route(conn, "GET", ~w(api v1 user), _, o, _p),
    do: json(conn, 200, %{"login" => o[:login] || "kbot"})

  defp route(conn, "GET", ~w(api v1 user actions runners jobs), _, _o, _p),
    do: json(conn, 200, [@solo])

  defp route(conn, "DELETE", ["api", "v1", "user", "actions", "runners", _], _, _o, _p),
    do: json(conn, 404, %{})

  defp route(conn, "GET", ~w(api v1 admin actions runners jobs), _, _o, _p),
    do: json(conn, 200, [hd(@org_jobs), @solo])

  defp route(conn, "DELETE", ["api", "v1", "admin", "actions", "runners", _], _, _o, _p),
    do: json(conn, 404, %{})

  defp route(conn, "GET", ~w(api v1 orgs lodge actions runners), _, o, _p) do
    page = String.to_integer(conn.query_params["page"])
    limit = String.to_integer(conn.query_params["limit"])
    limit = if max = o[:max_page_size], do: min(limit, max), else: limit

    json(
      conn,
      200,
      o |> Keyword.get(:runners, []) |> Enum.drop((page - 1) * limit) |> Enum.take(limit)
    )
  end

  defp route(conn, "POST", ~w(api v1 orgs lodge actions runners), _, _o, pid) do
    {:ok, body, conn} = Plug.Conn.read_body(conn)
    send(pid, {:registered, Jason.decode!(body)})
    json(conn, 201, %{"id" => 99, "uuid" => "u-1", "token" => "t-1"})
  end

  defp route(conn, "DELETE", ["api", "v1", "orgs", "lodge", "actions", "runners", id], _, _o, pid) do
    if id == "0" do
      json(conn, 404, %{})
    else
      send(pid, {:deleted, String.to_integer(id)})
      Plug.Conn.send_resp(conn, 204, "")
    end
  end

  defp route(conn, method, path, _, _o, _p),
    do: json(conn, 404, %{"message" => "no route #{method} /#{Enum.join(path, "/")}"})

  defp client(stub, scopes) do
    {:ok, c} =
      Client.new(
        url: @base <> "/",
        token: "secret",
        scopes: scopes,
        req_options: [plug: {Req.Test, stub}, retry: false]
      )

    c
  end

  defp lookups do
    receive do
      {:repo_lookup, _} -> 1 + lookups()
    after
      0 -> 0
    end
  end

  defp deleted do
    receive do
      {:deleted, id} -> [id | deleted()]
    after
      0 -> []
    end
  end

  defp others(n), do: for(i <- 0..(n - 1), do: %{"id" => i, "name" => "other-#{i}"})

  test "lists only waiting jobs by handle, and caches repo names" do
    forge = client(fake(), ["lodge"])
    assert {:ok, [job]} = Client.list_queued_jobs(forge)
    assert job.id == "h-wait"
    assert job.labels == ["linux"]
    assert job.repository == "lodge/app"
    assert job.runner_scope == {:org, "lodge"}
    assert {:ok, [_]} = Client.list_queued_jobs(forge)
    assert lookups() == 1
  end

  test "user and org scopes" do
    forge = client(fake(), ["@kbot", "lodge"])
    assert :ok = Client.validate(forge)
    {:ok, jobs} = Client.list_queued_jobs(forge)

    assert jobs |> Enum.sort_by(& &1.id) |> Enum.map(&{&1.id, &1.repository, &1.runner_scope}) ==
             [
               {"h-solo", "kbot/solo", {:user, "kbot"}},
               {"h-wait", "lodge/app", {:org, "lodge"}}
             ]

    # status checks go to the scope that owns the repository
    assert {:ok, "queued"} = Client.job_status(forge, "kbot/solo", "h-solo")
    assert {:ok, "queued"} = Client.job_status(forge, "lodge/app", "h-wait")
    assert Client.scope_url(forge, {:user, "kbot"}) == @base <> "/kbot"
    assert Client.scope_path({:user, "kbot"}) == "user"
  end

  test "a user scope must be the token's own user" do
    assert {:error, msg} = Client.validate(client(fake(login: "someone-else"), ["@kbot"]))
    assert msg =~ "token belongs to 'someone-else'"
  end

  test "an instance scope covers everything, once" do
    stub = fake()
    forge = client(stub, ["*", "lodge"])
    assert :ok = Client.validate(forge)
    {:ok, jobs} = Client.list_queued_jobs(forge)
    assert jobs |> Enum.map(& &1.id) |> Enum.sort() == ["h-solo", "h-wait"]
    assert Client.scope_url(forge, :instance) == @base
    assert Client.scope_path(:instance) == "admin"
    # a repository no configured org or user owns is asked about at the instance
    assert Client.scope_for_repo(client(stub, ["*"]), "kbot/solo") == {:ok, :instance}
    assert {:ok, "queued"} = Client.job_status(client(stub, ["*"]), "kbot/solo", "h-solo")
  end

  test "org, user and instance scopes need read:repository" do
    stub = fake(no_repo_read: true)

    for scopes <- [["*"], ["@kbot"], ["lodge"]] do
      assert {:error, msg} = Client.validate(client(stub, scopes))
      assert msg =~ "read:repository", inspect(scopes)
    end

    # a repo scope names its repository itself: it fails on its jobs (403), not the probe
    assert {:error, _} = Client.validate(client(stub, ["bad/repo"]))
  end

  test "validate needs runner write access" do
    stub = fake()
    assert :ok = Client.validate(client(stub, ["lodge"]))
    assert deleted() == []
    # a read-only token lists jobs fine but can't manage runners
    assert {:error, msg} = Client.validate(client(stub, ["ro"]))
    assert msg =~ "must own the scope"
  end

  test "null lists are empty" do
    forge = client(fake(), ["empty"])
    assert {:ok, []} = Client.list_queued_jobs(forge)
    # a finished job is "not found", not an error
    assert {:ok, nil} = Client.job_status(forge, "empty/app", "h-1")
    assert :ok = Client.remove_runner(forge, {:org, "empty"}, "runner-1")
  end

  test "one failing scope is skipped; all failing is an error" do
    stub = fake()
    assert {:ok, [_]} = Client.list_queued_jobs(client(stub, ["lodge", "bad/repo"]))
    assert {:error, _} = Client.list_queued_jobs(client(stub, ["bad/repo"]))
  end

  test "job status maps to GitHub terms" do
    forge = client(fake(), ["lodge"])
    assert {:ok, "queued"} = Client.job_status(forge, "lodge/app", "h-wait")
    assert {:ok, "in_progress"} = Client.job_status(forge, "Lodge/app", "h-run")
    assert {:ok, nil} = Client.job_status(forge, "lodge/app", "gone")
    # a repo outside the org scopes is asked at repo scope; errors aren't "not found"
    assert {:error, msg} = Client.job_status(forge, "bad/repo", "x")
    assert msg =~ "must own the scope"
  end

  test "registers an ephemeral runner for the handle" do
    forge = client(fake(), ["lodge"])

    assert {:ok, {:forgejo, launch}} =
             Forge.create_job_runner(
               {Client, forge},
               {:org, "lodge"},
               "runner-1",
               ["linux"],
               nil,
               "h-wait"
             )

    assert launch == %{instance_url: @base, uuid: "u-1", token: "t-1", handle: "h-wait"}
    assert_received {:registered, %{"name" => "runner-1", "ephemeral" => true}}
    assert Client.scope_url(forge, {:org, "lodge"}) == @base <> "/lodge"
    assert {:error, _} = Client.registration_token(forge, {:org, "lodge"})
  end

  test "removes a runner by name across pages" do
    runners = others(50) ++ [%{"id" => 500, "name" => "runner-1"}]
    forge = client(fake(runners: runners), ["lodge"])
    assert :ok = Client.remove_runner(forge, {:org, "lodge"}, "runner-1")
    assert deleted() == [500]
    # a runner that's already gone is fine
    assert :ok = Client.remove_runner(forge, {:org, "lodge"}, "missing")
    # one-job runners are never drained as idle pool runners
    assert {:ok, :busy} = Client.remove_idle_runner(forge, {:org, "lodge"}, "runner-1")
    assert {:ok, :not_registered} = Client.remove_idle_runner(forge, {:org, "lodge"}, "missing")
    assert deleted() == []
  end

  test "lists runners past short pages (server caps page size, FJ3)" do
    # a server with MAX_RESPONSE_ITEMS = 30 returns 30 per page even when asked for 50
    runners = others(50) ++ [%{"id" => 500, "name" => "runner-1"}]
    forge = client(fake(runners: runners, max_page_size: 30), ["lodge"])
    assert {:ok, listed} = Client.list_runners(forge, {:org, "lodge"})
    assert length(listed) == 51
    assert :ok = Client.remove_runner(forge, {:org, "lodge"}, "runner-1")
    assert deleted() == [500]
  end

  test "new/1 reads the token from token_env, token_path or token, in that order (FJ1)" do
    path = Path.join(System.tmp_dir!(), "fj-token-#{System.unique_integer([:positive])}")
    File.write!(path, "from-file\n")
    System.put_env("KPC_FJ_TEST_TOKEN", "from-env")

    on_exit(fn ->
      File.rm(path)
      System.delete_env("KPC_FJ_TEST_TOKEN")
    end)

    base = [url: @base, scopes: ["lodge"]]

    assert {:ok, %{token: "from-env"}} =
             Client.new(base ++ [token_env: "KPC_FJ_TEST_TOKEN", token_path: path, token: "t"])

    assert {:ok, %{token: "from-file"}} = Client.new(base ++ [token_path: path, token: "t"])
    assert {:ok, %{token: "t"}} = Client.new(base ++ [token: "t"])
    assert {:error, _} = Client.new(base ++ [token: ""])
    assert {:error, _} = Client.new(url: "ftp://x", token: "t", scopes: ["lodge"])
    assert {:error, _} = Client.new(url: @base, token: "t", scopes: [])
  end
end
