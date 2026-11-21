defmodule KuiperCoordinator.AgentsTest do
  use KuiperCoordinator.DataCase, async: true

  alias KuiperCoordinator.{Agents, Repo}
  alias KuiperCoordinator.Agents.{Agent, Bundle, RegistrationToken}
  import Ecto.Query

  test "tokens are reg_ + 32 alphanumerics" do
    {:ok, t} = Agents.create_registration_token(60)
    assert t.token =~ ~r/\Areg_[A-Za-z0-9]{32}\z/
  end

  # tokens are bearer secrets: they must come from a CSPRNG, not the seedable :rand
  test "tokens don't repeat when :rand is reseeded the same way" do
    tokens =
      for _ <- 1..2 do
        :rand.seed(:exsss, {1, 2, 3})
        {:ok, t} = Agents.create_registration_token(60)
        t.token
      end

    assert [a, b] = tokens
    refute a == b
  end

  # bundle.rs format: kfr1_ + base64url(no pad) of {"t","ca","m","u"}
  test "registration bundles use the kuiper-agent-lib bundle format" do
    s = Bundle.encode("reg_abc", "-----BEGIN CERTIFICATE-----\n", "ca", "https://c.example:9543")
    assert "kfr1_" <> payload = s
    refute payload =~ "="

    assert Jason.decode!(Base.url_decode64!(payload, padding: false)) == %{
             "t" => "reg_abc",
             "ca" => "-----BEGIN CERTIFICATE-----\n",
             "m" => "ca",
             "u" => "https://c.example:9543"
           }

    # chain mode with public roots leaves "ca" out, like skip_serializing_if
    assert {:ok, map} = Bundle.decode(Bundle.encode("reg_abc", nil, "chain", "https://c"))
    refute Map.has_key?(map, "ca")
  end

  # L2
  test "truncate cuts at a UTF-8 boundary and marks it" do
    assert Agents.truncate("short", 10) == "short"
    s = "aé" <> String.duplicate("b", 10)
    # byte 2 is the middle of é
    assert Agents.truncate(s, 2) == "a …[truncated]"
    assert Agents.truncate(s, 3) == "aé …[truncated]"
    long = String.duplicate("x", 16 * 1024 + 5)
    assert byte_size(Agents.truncate(long, 16 * 1024)) == 16 * 1024 + byte_size(" …[truncated]")
  end

  test "levels map ERROR=1..TRACE=5, unknown as INFO" do
    assert Enum.map(~w(ERROR warn Info debug TRACE nope), &Agents.level/1) == [1, 2, 3, 4, 5, 3]
  end

  # A13
  test "purge drops expired tokens and agents revoked over 7 days ago only" do
    {:ok, live} = Agents.create_registration_token(600)
    {:ok, _expired} = Agents.create_registration_token(-10)
    now = DateTime.utc_now()

    for {id, revoked} <- [
          {"agent_a", nil},
          {"agent_b", DateTime.add(now, -1, :day)},
          {"agent_c", DateTime.add(now, -8, :day)}
        ] do
      Repo.insert!(%Agent{
        agent_id: id,
        hostname: "h",
        agent_type: "tart",
        serial_number: "00",
        expires_at: DateTime.add(now, 300, :day),
        revoked_at: revoked
      })
    end

    assert %{tokens: 1, agents: 1} = Agents.purge_expired()
    assert Repo.get(RegistrationToken, live.token)
    assert Repo.get(Agent, "agent_a")
    assert Repo.get(Agent, "agent_b")
    refute Repo.get(Agent, "agent_c")
  end

  test "an agent whose cert has expired is not valid" do
    Repo.insert!(%Agent{
      agent_id: "agent_old",
      hostname: "h",
      agent_type: "tart",
      serial_number: "00",
      expires_at: DateTime.add(DateTime.utc_now(), -1, :day)
    })

    refute Agents.valid_agent?("agent_old")
    refute Agents.valid_agent?("agent_missing")
  end

  describe "log retention" do
    alias KuiperCoordinator.Agents.LogLine
    import Ecto.Query

    defp records(n, from) do
      for i <- from..(from + n - 1) do
        %Cirunner.Agent.LogRecord{
          timestamp_micros: i,
          seq: i,
          level: "INFO",
          target: "t",
          message: "line #{i}"
        }
      end
    end

    defp messages(agent_id), do: Enum.map(Agents.log_lines(agent_id), & &1.message)

    test "repeated uploads keep only the newest lines per agent" do
      Application.put_env(:kuiper_coordinator, :agent_log_max_lines, 5)
      on_exit(fn -> Application.delete_env(:kuiper_coordinator, :agent_log_max_lines) end)

      for batch <- 0..2, do: :ok = Agents.insert_log_batch("agent_a", records(4, batch * 4), 0)
      :ok = Agents.insert_log_batch("agent_b", records(2, 0), 0)

      assert messages("agent_a") == for(i <- 7..11, do: "line #{i}")
      assert length(messages("agent_b")) == 2
    end

    # by insert order: an agent can't keep its lines by sending old or future timestamps
    test "the cap goes by insert order, not the agent's timestamps" do
      Application.put_env(:kuiper_coordinator, :agent_log_max_lines, 2)
      on_exit(fn -> Application.delete_env(:kuiper_coordinator, :agent_log_max_lines) end)

      :ok = Agents.insert_log_batch("agent_c", records(2, 9_000_000), 0)
      :ok = Agents.insert_log_batch("agent_c", records(2, 1), 0)
      assert messages("agent_c") == ["line 1", "line 2"]
    end

    test "prune_logs drops lines stored more than 7 days ago" do
      :ok = Agents.insert_log_batch("agent_d", records(3, 0), 0)

      [old_id | _] =
        Repo.all(from(l in LogLine, where: l.agent_id == "agent_d", order_by: l.id, select: l.id))

      long_ago = DateTime.add(DateTime.utc_now(), -8, :day)
      Repo.update_all(from(l in LogLine, where: l.id == ^old_id), set: [inserted_at: long_ago])

      assert %{old: 1} = Agents.prune_logs()
      assert messages("agent_d") == ["line 1", "line 2"]
    end
  end

  test "a failed agent insert gives the registration token back" do
    {ca_cert, ca_key} = KuiperCoordinator.AgentGrpcCase.fixture_ca()
    KuiperCoordinator.CA.install!(ca_cert, ca_key)
    {:ok, t} = Agents.create_registration_token(60)
    # a nil hostname makes issuing raise inside the transaction
    assert catch_error(Agents.register(t.token, nil, "tart"))
    assert Repo.get_by(RegistrationToken, token: t.token)
    assert {:ok, %{agent_id: _}} = Agents.register(t.token, "mac-mini-1", "tart")
  end

  test "an expired token stays burned" do
    {:ok, t} = Agents.create_registration_token(60)

    Repo.update_all(from(r in RegistrationToken, where: r.token == ^t.token),
      set: [expires_at: DateTime.add(DateTime.utc_now(), -60)]
    )

    assert {:error, :unauthenticated, "Registration token has expired"} =
             Agents.register(t.token, "mac-mini-1", "tart")

    refute Repo.get_by(RegistrationToken, token: t.token)
  end

  test "a log batch is stored whole or not at all" do
    records =
      for i <- 1..1_500 do
        # seq past bigint makes the second 1,000-row chunk fail
        seq = if i == 1_200, do: 9_223_372_036_854_775_808, else: i

        %Cirunner.Agent.LogRecord{
          timestamp_micros: i,
          seq: seq,
          level: "INFO",
          target: "t",
          message: "m"
        }
      end

    assert_raise DBConnection.EncodeError, fn ->
      Agents.insert_log_batch("agent_atomic", records, 0)
    end

    assert Agents.log_lines("agent_atomic") == []
  end

  test "a hostname near the DNS maximum still registers, with the id cut to the CN limit" do
    {ca_cert, ca_key} = KuiperCoordinator.AgentGrpcCase.fixture_ca()
    KuiperCoordinator.CA.install!(ca_cert, ca_key)
    {:ok, t} = Agents.create_registration_token(60)
    long_host = String.duplicate("a", 63) <> "." <> String.duplicate("b", 63) <> ".example.com"

    assert {:ok, %{agent_id: id}} = Agents.register(t.token, long_host, "libvirt")
    assert String.length(id) == 64
    assert id =~ ~r/\Aagent_libvirt_a+_[A-Za-z0-9]{8}\z/
  end
end
