defmodule KuiperCoordinator.GitHub.ClientQueuedJobsTest do
  # Recovery scan (parity G9): github.rs list_queued_jobs / find_queued_jobs_in_repo.
  use ExUnit.Case, async: true

  alias KuiperCoordinator.Forge.QueuedJob
  alias KuiperCoordinator.GitHub.Client

  @now ~U[2026-11-21 12:00:00Z]

  defp client(opts \\ []) do
    key = :public_key.generate_key({:rsa, 2048, 65_537})
    pem = :public_key.pem_encode([:public_key.pem_entry_encode(:RSAPrivateKey, key)])
    {:ok, cache} = Client.Cache.start_link()

    Client.new(
      [
        app_id: 99,
        private_key: pem,
        cache: cache,
        now: fn -> @now end,
        req_options: [plug: {Req.Test, __MODULE__}, retry: false]
      ] ++ opts
    )
  end

  defp repo(name, pushed_at),
    do: %{
      "full_name" => "acme/#{name}",
      "owner" => %{"login" => "acme"},
      "pushed_at" => pushed_at
    }

  defp job(id, status, labels \\ ["self-hosted"]),
    do: %{
      "id" => id,
      "status" => status,
      "labels" => labels,
      "created_at" => "2026-11-21T11:00:00Z"
    }

  # A fake GitHub with one installation (acme, id 7). `repos` is the installation's repo list
  # (or a status), `runs` maps {repo, run status} to runs, `jobs` maps run id to jobs.
  defp stub(opts) do
    test_pid = self()
    repos = Keyword.get(opts, :repos, [])
    runs = Keyword.get(opts, :runs, %{})
    jobs = Keyword.get(opts, :jobs, %{})
    token_status = Keyword.get(opts, :token_status, 201)

    Req.Test.stub(__MODULE__, fn conn ->
      q = URI.decode_query(conn.query_string)
      send(test_pid, {:req, conn.request_path, q})

      case String.split(conn.request_path, "/", trim: true) do
        ["app", "installations"] ->
          Req.Test.json(conn, [%{"id" => 7, "account" => %{"login" => "acme"}}])

        ["app", "installations", "7", "access_tokens"] ->
          conn
          |> Plug.Conn.put_status(token_status)
          |> Req.Test.json(%{"token" => "ghs_1", "expires_at" => "2026-11-21T13:00:00Z"})

        ["installation", "repositories"] ->
          case repos do
            status when is_integer(status) ->
              conn |> Plug.Conn.put_status(status) |> Req.Test.text("boom")

            list ->
              page = String.to_integer(q["page"] || "1")
              per = String.to_integer(q["per_page"] || "100")
              Req.Test.json(conn, %{"repositories" => Enum.slice(list, (page - 1) * per, per)})
          end

        ["repos", owner, name, "actions", "runs"] ->
          case Map.get(runs, {"#{owner}/#{name}", q["status"]}, []) do
            status when is_integer(status) ->
              conn |> Plug.Conn.put_status(status) |> Req.Test.text("boom")

            list ->
              Req.Test.json(conn, %{"workflow_runs" => Enum.map(list, &%{"id" => &1})})
          end

        ["repos", _owner, _name, "actions", "runs", run_id, "jobs"] ->
          case Map.get(jobs, String.to_integer(run_id), []) do
            status when is_integer(status) ->
              conn |> Plug.Conn.put_status(status) |> Req.Test.text("boom")

            list ->
              Req.Test.json(conn, %{"jobs" => list})
          end
      end
    end)
  end

  defp requested_paths do
    receive do
      {:req, path, _q} -> [path | requested_paths()]
    after
      0 -> []
    end
  end

  test "finds queued and waiting jobs in every run status, org-scoped" do
    stub(
      repos: [repo("app", "2026-11-20T00:00:00Z")],
      runs: %{{"acme/app", "queued"} => [11], {"acme/app", "in_progress"} => [12]},
      jobs: %{
        11 => [job(101, "queued", ["self-hosted", "macos"]), job(102, "completed")],
        12 => [job(103, "in_progress"), job(104, "waiting")]
      }
    )

    assert {:ok, jobs} = Client.list_queued_jobs(client())

    assert [
             %QueuedJob{
               id: "101",
               labels: ["self-hosted", "macos"],
               runner_scope: {:org, "acme"},
               repository: "acme/app",
               created_at: ~U[2026-11-21 11:00:00Z]
             },
             %QueuedJob{id: "104", runner_scope: {:org, "acme"}}
           ] = Enum.sort_by(jobs, & &1.id)

    paths = requested_paths()
    assert "/repos/acme/app/actions/runs" in paths
  end

  test "lookback skips repos not pushed in 7 days; unparseable or missing dates are scanned" do
    stub(
      repos: [
        repo("old", "2026-11-01T00:00:00Z"),
        repo("nodate", nil),
        repo("bad", "yesterday")
      ],
      runs: %{
        {"acme/old", "queued"} => [1],
        {"acme/nodate", "queued"} => [2],
        {"acme/bad", "queued"} => [3]
      },
      jobs: %{1 => [job(1, "queued")], 2 => [job(2, "queued")], 3 => [job(3, "queued")]}
    )

    assert {:ok, jobs} = Client.list_queued_jobs(client())
    assert jobs |> Enum.map(& &1.id) |> Enum.sort() == ["2", "3"]
    refute "/repos/acme/old/actions/runs" in requested_paths()
  end

  test "a lookback of 0 scans every repo; a custom one moves the cutoff" do
    stub(
      repos: [repo("old", "2026-11-01T00:00:00Z")],
      runs: %{{"acme/old", "queued"} => [1]},
      jobs: %{1 => [job(1, "queued")]}
    )

    assert {:ok, [%QueuedJob{id: "1"}]} =
             Client.list_queued_jobs(client(scan_lookback_hours: 0))

    assert {:ok, [%QueuedJob{id: "1"}]} =
             Client.list_queued_jobs(client(scan_lookback_hours: 24 * 30))

    assert {:ok, []} = Client.list_queued_jobs(client(scan_lookback_hours: 24))
  end

  test "reads every page of the installation's repos" do
    many = for i <- 1..150, do: repo("r#{i}", "2026-11-20T00:00:00Z")

    stub(
      repos: many,
      runs: %{{"acme/r150", "queued"} => [9]},
      jobs: %{9 => [job(9, "queued")]}
    )

    assert {:ok, [%QueuedJob{id: "9", repository: "acme/r150"}]} =
             Client.list_queued_jobs(client())
  end

  test "a failed run or job listing skips only that listing" do
    stub(
      repos: [repo("app", "2026-11-20T00:00:00Z")],
      runs: %{
        {"acme/app", "queued"} => 500,
        {"acme/app", "in_progress"} => [12, 13]
      },
      jobs: %{12 => 502, 13 => [job(130, "queued")]}
    )

    assert {:ok, [%QueuedJob{id: "130"}]} = Client.list_queued_jobs(client())
  end

  test "an installation whose token or repo list fails is skipped, not an error" do
    stub(repos: [repo("app", "2026-11-20T00:00:00Z")], token_status: 500)
    assert {:ok, []} = Client.list_queued_jobs(client())

    stub(repos: 500)
    assert {:ok, []} = Client.list_queued_jobs(client())
  end
end
