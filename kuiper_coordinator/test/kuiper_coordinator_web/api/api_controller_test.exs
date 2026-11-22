defmodule KuiperCoordinatorWeb.ApiControllerTest do
  @moduledoc """
  The read-only JSON API (parity API1-API4, admin/api.rs): bearer `kfapi_` tokens only, /status,
  /agents and /agents/:id/logs with a `before` cursor.
  """
  use KuiperCoordinatorWeb.ConnCase, async: false
  @moduletag :capture_log

  alias Cirunner.Agent.{AgentStatus, CapacityLimit, LabelSet}
  alias KuiperCoordinator.{Admin, AgentRegistry, Agents, Jobs, Repo, Runners}
  alias KuiperCoordinator.Admin.ApiTokens

  setup %{conn: conn} do
    old = Application.get_env(:kuiper_coordinator, :admin)
    Application.put_env(:kuiper_coordinator, :admin, session_timeout_secs: 3600)
    on_exit(fn -> Application.put_env(:kuiper_coordinator, :admin, old || []) end)
    {:ok, token, _} = ApiTokens.create("ci", "alice")
    %{conn: conn, token: token, authed: put_req_header(conn, "authorization", "Bearer #{token}")}
  end

  defp agent!(id, attrs \\ %{}) do
    Repo.insert!(
      struct(
        %Agents.Agent{
          agent_id: id,
          hostname: "#{id}.local",
          agent_type: "tart",
          labels: ["self-hosted", "macos"],
          max_vms: 2,
          serial_number: id,
          expires_at: ~U[2027-11-21 00:00:00.000000Z]
        },
        attrs
      )
    )
  end

  defp connect!(id, status) do
    test = self()

    pid =
      spawn_link(fn ->
        :ok =
          AgentRegistry.register(id, %{
            hostname: id,
            agent_type: "tart",
            status: status,
            vm_names: []
          })

        send(test, :registered)
        receive do: (:stop -> :ok)
      end)

    assert_receive :registered
    on_exit(fn -> send(pid, :stop) end)
  end

  describe "auth (API1)" do
    test "no bearer token is a 401 with WWW-Authenticate", %{conn: conn} do
      conn = get(conn, ~p"/api/v1/status")
      assert json_response(conn, 401) == %{"error" => "missing bearer token"}
      assert get_resp_header(conn, "www-authenticate") == ["Bearer"]
    end

    test "a wrong or deleted token is a 401", %{conn: conn, token: token} do
      bad = put_req_header(conn, "authorization", "Bearer kfapi_nope")
      assert json_response(get(bad, ~p"/api/v1/agents"), 401) == %{"error" => "invalid token"}

      [t] = ApiTokens.list()
      :ok = ApiTokens.delete(t.id)
      gone = put_req_header(build_conn(), "authorization", "Bearer #{token}")
      assert json_response(get(gone, ~p"/api/v1/status"), 401)["error"] == "invalid token"
    end

    test "an admin session cookie isn't enough", %{conn: conn} do
      {:ok, _} = Admin.create_user("alice", "pw", "pw")
      conn = post(conn, ~p"/admin/login", %{"username" => "alice", "password" => "pw"})
      conn = conn |> recycle() |> get(~p"/api/v1/status")
      assert json_response(conn, 401)["error"] == "missing bearer token"
    end

    test "is served with no admin config at all (D49), bearer token still required",
         %{conn: conn, authed: authed} do
      Application.delete_env(:kuiper_coordinator, :admin)
      assert json_response(get(authed, ~p"/api/v1/status"), 200)
      assert json_response(get(conn, ~p"/api/v1/status"), 401)
    end
  end

  test "GET /status counts agents, capacity, runners and pending jobs (API2)", %{authed: authed} do
    agent!("a_on", %{max_vms: 3})
    agent!("a_off")
    agent!("a_revoked", %{revoked_at: DateTime.utc_now()})
    agent!("a_away", %{unavailable_at: DateTime.utc_now()})
    connect!("a_on", %AgentStatus{max_vms: 3, labels: ["macos"]})

    {:ok, _} =
      Runners.add_runner(%{
        runner_name: "r1",
        agent_id: "a_on",
        vm_name: "vm1",
        runner_scope: {:org, "astro"},
        forge: "github",
        job_key: "github:1"
      })

    {:ok, _} =
      Jobs.add_pending(%{job_key: "github:2", forge: "github", runner_scope: {:org, "astro"}})

    assert json_response(get(authed, ~p"/api/v1/status"), 200) == %{
             "registered_agents" => 4,
             "connected_agents" => 1,
             "revoked_agents" => 1,
             "unavailable_agents" => 1,
             "total_capacity" => 3,
             "active_runners" => 1,
             "pending_jobs" => 1
           }
  end

  test "GET /agents lists each agent with live status (API3)", %{authed: authed} do
    agent!("a_on", %{agent_version: "0.9.1"})
    agent!("a_off", %{unavailable_at: ~U[2026-11-20 10:00:00.000000Z]})
    agent!("a_revoked", %{revoked_at: DateTime.utc_now()})
    connect!("a_revoked", %AgentStatus{})

    connect!("a_on", %AgentStatus{
      labels: ["self-hosted", "macos"],
      label_sets: [%LabelSet{labels: ["self-hosted", "macos", "xcode"]}],
      limits: [%CapacityLimit{name: "macos", max: 2, external: 1, active: 1}]
    })

    agents = json_response(get(authed, ~p"/api/v1/agents"), 200)
    by_id = Map.new(agents, &{&1["agent_id"], &1})

    on = by_id["a_on"]
    assert on["status"] == "online"
    assert on["hostname"] == "a_on.local"
    assert on["agent_type"] == "tart"
    assert on["labels"] == ["self-hosted", "macos"]
    assert on["label_sets"] == [["self-hosted", "macos", "xcode"]]
    assert on["limits"] == [%{"name" => "macos", "max" => 2, "external" => 1, "active" => 1}]
    assert on["version"] == "0.9.1"
    assert on["unavailable"] == false
    assert on["unavailable_since"] == nil
    assert is_integer(on["last_seen_secs"])
    assert on["cert_expires_at"] == "2027-11-21T00:00:00.000000Z"

    off = by_id["a_off"]
    assert off["status"] == "offline"
    assert off["unavailable"] == true
    assert off["unavailable_since"] == "2026-11-20T10:00:00.000000Z"
    assert off["label_sets"] == [] and off["limits"] == [] and off["last_seen_secs"] == nil
    assert off["version"] == nil

    # revoked wins over online
    assert by_id["a_revoked"]["status"] == "revoked"
  end

  describe "GET /agents/:id/logs (API4)" do
    defp logs!(agent_id, n) do
      rows =
        for i <- 1..n do
          %{
            timestamp_micros: 1_000 + i,
            seq: i,
            level: if(rem(i, 2) == 0, do: "ERROR", else: "DEBUG"),
            target: "kuiper",
            message: "line #{i}"
          }
        end

      Agents.insert_log_batch(agent_id, rows, 0)
    end

    test "newest first, pages with next_before, filters by level", %{authed: authed} do
      agent!("a1")
      logs!("a1", 5)

      page = json_response(get(authed, ~p"/api/v1/agents/a1/logs?limit=2"), 200)
      assert Enum.map(page["lines"], & &1["message"]) == ["line 5", "line 4"]
      assert hd(page["lines"])["level"] == "debug"
      assert hd(page["lines"])["target"] == "kuiper"
      assert hd(page["lines"])["ts"] == "1970-01-01T00:00:00.001005Z"
      assert page["next_before"] == "1004_4"

      page2 =
        json_response(get(authed, ~p"/api/v1/agents/a1/logs?limit=2&before=1004_4"), 200)

      assert Enum.map(page2["lines"], & &1["message"]) == ["line 3", "line 2"]

      last = json_response(get(authed, ~p"/api/v1/agents/a1/logs?before=1002_2"), 200)
      assert Enum.map(last["lines"], & &1["message"]) == ["line 1"]
      assert last["next_before"] == nil

      errors = json_response(get(authed, ~p"/api/v1/agents/a1/logs?level=error"), 200)
      assert Enum.map(errors["lines"], & &1["message"]) == ["line 4", "line 2"]
    end

    test "malformed params are ignored, not a 500 (F15)", %{authed: authed} do
      agent!("a1")
      logs!("a1", 3)

      for query <- [
            "before=99999999999999999999999_1",
            "before=1_99999999999999999999999",
            "before=-99999999999999999999999_1",
            "before[]=1002_2",
            "level[]=x",
            "level[a]=error",
            "limit[]=2",
            "limit[a]=2"
          ] do
        conn = get(authed, "/api/v1/agents/a1/logs?" <> query)
        assert length(json_response(conn, 200)["lines"]) == 3, query
      end
    end

    test "limit clamps to 1..1000, a bad cursor or unknown agent is ignored", %{authed: authed} do
      agent!("a1")
      logs!("a1", 3)

      assert length(json_response(get(authed, ~p"/api/v1/agents/a1/logs?limit=0"), 200)["lines"]) ==
               1

      assert length(json_response(get(authed, ~p"/api/v1/agents/a1/logs?limit=x"), 200)["lines"]) ==
               3

      assert length(
               json_response(get(authed, ~p"/api/v1/agents/a1/logs?before=nope"), 200)["lines"]
             ) == 3

      assert json_response(get(authed, ~p"/api/v1/agents/ghost/logs"), 200) == %{
               "lines" => [],
               "next_before" => nil
             }
    end
  end
end
