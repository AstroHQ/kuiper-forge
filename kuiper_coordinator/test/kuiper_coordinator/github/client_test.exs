defmodule KuiperCoordinator.GitHub.ClientTest do
  use ExUnit.Case, async: true

  alias KuiperCoordinator.GitHub.Client

  @now ~U[2026-11-21 12:00:00Z]

  setup do
    key = :public_key.generate_key({:rsa, 2048, 65_537})
    pem = :public_key.pem_encode([:public_key.pem_entry_encode(:RSAPrivateKey, key)])
    test_pid = self()
    {:ok, cache} = Client.Cache.start_link()

    client =
      Client.new(
        app_id: 99,
        private_key: pem,
        cache: cache,
        now: fn -> Process.get(:now, @now) end,
        req_options: [plug: {Req.Test, __MODULE__}, retry: false]
      )

    {:ok, client: client, test_pid: test_pid}
  end

  # A fake GitHub: one installation "acme" (id 7), tokens that expire in `ttl` seconds.
  defp stub_github(test_pid, routes, ttl \\ 3600) do
    Req.Test.stub(__MODULE__, fn conn ->
      send(test_pid, {:req, conn.method, conn.request_path, conn.query_string, conn})

      case {conn.method, conn.request_path} do
        {"GET", "/app/installations"} ->
          Req.Test.json(conn, [%{"id" => 7, "account" => %{"login" => "Acme"}}])

        {"POST", "/app/installations/7/access_tokens"} ->
          n = System.unique_integer([:positive])
          exp = DateTime.add(Process.get(:now, @now), ttl) |> DateTime.to_iso8601()

          conn
          |> Plug.Conn.put_status(201)
          |> Req.Test.json(%{"token" => "ghs_#{n}", "expires_at" => exp})

        key ->
          case Map.fetch(routes, key) do
            {:ok, fun} ->
              fun.(conn)

            :error ->
              conn |> Plug.Conn.put_status(599) |> Req.Test.text("unexpected #{inspect(key)}")
          end
      end
    end)
  end

  defp token_posts do
    for {:req, "POST", "/app/installations/7/access_tokens", _, _} <- drain(), do: 1
  end

  defp drain(acc \\ []) do
    receive do
      {:req, _, _, _, _} = m -> drain([m | acc])
    after
      0 -> Enum.reverse(acc)
    end
  end

  defp reg_token_route do
    %{
      {"POST", "/orgs/acme/actions/runners/registration-token"} => fn conn ->
        conn
        |> Plug.Conn.put_status(201)
        |> Req.Test.json(%{"token" => "REG", "expires_at" => "2026-11-21T13:00:00Z"})
      end
    }
  end

  test "registration token for an org scope, with GitHub headers (G4)", %{client: c, test_pid: p} do
    stub_github(p, reg_token_route())
    assert {:ok, "REG"} = Client.registration_token(c, {:org, "acme"})

    [{:req, _, _, _, conn}] =
      for {:req, "POST", "/orgs/acme/actions/runners/registration-token", _, _} = m <- drain(),
          do: m

    assert ["Bearer ghs_" <> _] = Plug.Conn.get_req_header(conn, "authorization")
    assert ["application/vnd.github+json"] = Plug.Conn.get_req_header(conn, "accept")
    assert ["2022-11-28"] = Plug.Conn.get_req_header(conn, "x-github-api-version")
    assert ["kuiper-forge"] = Plug.Conn.get_req_header(conn, "user-agent")
  end

  test "installation token is reused while it has more than 5 minutes left (G3)", %{
    client: c,
    test_pid: p
  } do
    stub_github(p, reg_token_route())
    {:ok, _} = Client.registration_token(c, {:org, "acme"})
    {:ok, _} = Client.registration_token(c, {:org, "acme"})
    assert token_posts() == [1]
  end

  test "a token expiring in 4 minutes is refreshed (port of github.rs cached token test)", %{
    client: c,
    test_pid: p
  } do
    stub_github(p, reg_token_route(), 240)
    {:ok, _} = Client.registration_token(c, {:org, "acme"})
    {:ok, _} = Client.registration_token(c, {:org, "acme"})
    assert token_posts() == [1, 1]
  end

  test "installation accounts match case-insensitively; unknown account is an error", %{
    client: c,
    test_pid: p
  } do
    upper = %{
      {"POST", "/orgs/ACME/actions/runners/registration-token"} => fn conn ->
        conn |> Plug.Conn.put_status(201) |> Req.Test.json(%{"token" => "REG"})
      end
    }

    stub_github(p, upper)
    assert {:ok, "REG"} = Client.registration_token(c, {:org, "ACME"})
    assert {:error, {:not_installed, "other"}} = Client.registration_token(c, {:org, "other"})
  end

  test "user and instance scopes are refused for GitHub", %{client: c} do
    assert {:error, {:unsupported_scope, _}} = Client.registration_token(c, {:user, "jfro"})
    assert {:error, {:unsupported_scope, _}} = Client.registration_token(c, :instance)
  end

  test "JIT config body: name, runner_group_id, labels, work_folder (G6)", %{
    client: c,
    test_pid: p
  } do
    routes = %{
      {"POST", "/repos/acme/app/actions/runners/generate-jitconfig"} => fn conn ->
        {:ok, body, conn} = Plug.Conn.read_body(conn)
        send(p, {:jit_body, Jason.decode!(body)})

        conn
        |> Plug.Conn.put_status(201)
        |> Req.Test.json(%{"runner" => %{"id" => 5}, "encoded_jit_config" => "JIT"})
      end
    }

    stub_github(p, routes)

    assert {:ok, "JIT"} =
             Client.jit_config(
               c,
               {:repo, "acme", "app"},
               "pve01-abcd1234",
               ["self-hosted", "linux"],
               3
             )

    assert_received {:jit_body, body}

    assert body == %{
             "name" => "pve01-abcd1234",
             "runner_group_id" => 3,
             "labels" => ["self-hosted", "linux"],
             "work_folder" => "_work"
           }
  end

  defp runners_route(runners) do
    {{"GET", "/orgs/acme/actions/runners"},
     fn conn ->
       name = Plug.Conn.fetch_query_params(conn).query_params["name"]

       Req.Test.json(conn, %{
         "total_count" => 1,
         "runners" => Enum.filter(runners, &(&1["name"] == name))
       })
     end}
  end

  defp delete_route(status) do
    {{"DELETE", "/orgs/acme/actions/runners/11"},
     fn conn -> conn |> Plug.Conn.put_status(status) |> Req.Test.text("") end}
  end

  test "remove_idle_runner: absent -> not_registered, busy -> busy, idle -> removed (G7)", %{
    client: c,
    test_pid: p
  } do
    stub_github(p, Map.new([runners_route([]), delete_route(204)]))
    assert {:ok, :not_registered} = Client.remove_idle_runner(c, {:org, "acme"}, "r1")

    stub_github(
      p,
      Map.new([
        runners_route([%{"id" => 11, "name" => "r1", "busy" => true, "status" => "online"}]),
        delete_route(204)
      ])
    )

    assert {:ok, :busy} = Client.remove_idle_runner(c, {:org, "acme"}, "r1")

    stub_github(
      p,
      Map.new([
        runners_route([%{"id" => 11, "name" => "r1", "busy" => false, "status" => "online"}]),
        delete_route(204)
      ])
    )

    assert {:ok, :removed} = Client.remove_idle_runner(c, {:org, "acme"}, "r1")
  end

  test "a 422 on delete means busy, not an error, for idle removal (G7)", %{
    client: c,
    test_pid: p
  } do
    stub_github(
      p,
      Map.new([
        runners_route([%{"id" => 11, "name" => "r1", "busy" => false, "status" => "online"}]),
        delete_route(422)
      ])
    )

    assert {:ok, :busy} = Client.remove_idle_runner(c, {:org, "acme"}, "r1")
    # plain removal treats the refusal as an error
    assert {:error, {:runner_busy, 11}} = Client.remove_runner(c, {:org, "acme"}, "r1")
  end

  test "runner lookup is an exact name match", %{client: c, test_pid: p} do
    routes = %{
      {"GET", "/orgs/acme/actions/runners"} => fn conn ->
        Req.Test.json(conn, %{
          "runners" => [
            %{"id" => 12, "name" => "r1-extra", "busy" => false, "status" => "online"}
          ]
        })
      end
    }

    stub_github(p, routes)
    assert {:ok, :not_registered} = Client.remove_idle_runner(c, {:org, "acme"}, "r1")
    assert :ok = Client.remove_runner(c, {:org, "acme"}, "r1")
  end

  test "job status; 404 is nil (G8)", %{client: c, test_pid: p} do
    routes = %{
      {"GET", "/repos/acme/app/actions/jobs/1"} => fn conn ->
        Req.Test.json(conn, %{"status" => "queued"})
      end,
      {"GET", "/repos/acme/app/actions/jobs/2"} => fn conn ->
        conn |> Plug.Conn.put_status(404) |> Req.Test.json(%{})
      end,
      {"GET", "/repos/acme/app/actions/jobs/3"} => fn conn ->
        conn |> Plug.Conn.put_status(500) |> Req.Test.text("boom")
      end
    }

    stub_github(p, routes)
    assert {:ok, "queued"} = Client.job_status(c, "acme/app", 1)
    assert {:ok, nil} = Client.job_status(c, "acme/app", 2)
    assert {:error, {:http, 500, _}} = Client.job_status(c, "acme/app", 3)
    assert {:error, :invalid_repo} = Client.job_status(c, "acme", 3)
  end

  describe "runner groups (D9: unknown names fail loudly, no silent group-1 fallback)" do
    setup %{test_pid: p} do
      groups =
        for i <- 1..150 do
          %{"id" => i, "name" => if(i == 1, do: "Default", else: "group-#{i}")}
        end

      routes = %{
        {"GET", "/orgs/acme/actions/runner-groups"} => fn conn ->
          q = Plug.Conn.fetch_query_params(conn).query_params
          page = String.to_integer(q["page"] || "1")
          per = String.to_integer(q["per_page"] || "30")
          slice = groups |> Enum.drop((page - 1) * per) |> Enum.take(per)
          Req.Test.json(conn, %{"total_count" => 150, "runner_groups" => slice})
        end
      }

      stub_github(p, routes)
      :ok
    end

    test "nil is the Default group, a numeric string is an id", %{client: c} do
      assert {:ok, 1} = Client.resolve_runner_group(c, {:org, "acme"}, nil)
      assert {:ok, 42} = Client.resolve_runner_group(c, {:org, "acme"}, "42")
      assert {:ok, 42} = Client.resolve_runner_group(c, {:org, "acme"}, 42)
    end

    test "a name resolves through the runner-groups API, across pages", %{client: c} do
      assert {:ok, 1} = Client.resolve_runner_group(c, {:org, "acme"}, "Default")
      assert {:ok, 140} = Client.resolve_runner_group(c, {:org, "acme"}, "group-140")
      # repo scopes use the owner's org groups
      assert {:ok, 7} = Client.resolve_runner_group(c, {:repo, "acme", "app"}, "group-7")
    end

    test "an unknown name is an error, never group 1", %{client: c} do
      assert {:error, {:unknown_runner_group, "nope"}} =
               Client.resolve_runner_group(c, {:org, "acme"}, "nope")
    end
  end

  test "installations list is paged (G2, G5)", %{client: c, test_pid: p} do
    Req.Test.stub(__MODULE__, fn conn ->
      send(p, {:req, conn.method, conn.request_path, conn.query_string, conn})

      case {conn.method, conn.request_path} do
        {"GET", "/app/installations"} ->
          page = String.to_integer(Plug.Conn.fetch_query_params(conn).query_params["page"])
          ids = if page == 1, do: 1..100, else: 101..105

          Req.Test.json(
            conn,
            for(i <- ids, do: %{"id" => i, "account" => %{"login" => "acct#{i}"}})
          )
      end
    end)

    assert {:ok, accounts} = Client.validate(c)
    assert length(accounts) == 105
    assert "acct105" in accounts
  end
end
