defmodule KuiperCoordinator.RustImport.Source do
  @moduledoc """
  Read-only access to a Rust coordinator database: SQLite (`sqlite://PATH`, `sqlite:PATH`) or
  Postgres (`postgres://` / `postgresql://`). Rows come back as maps with string keys. Error
  messages never carry the URL's password.
  """
  alias Exqlite.Sqlite3

  @type t :: {:sqlite, reference()} | {:postgres, pid()}

  @spec open(String.t()) :: {:ok, t()} | {:error, String.t()}
  def open("sqlite://" <> path), do: open_sqlite(path)
  def open("sqlite:" <> path), do: open_sqlite(path)
  def open("postgres://" <> _ = url), do: open_postgres(url)
  def open("postgresql://" <> _ = url), do: open_postgres(url)

  def open(other) when is_binary(other),
    do:
      {:error,
       "can't read #{inspect(describe(other))}: use sqlite://PATH or postgres://USER@HOST/DB"}

  defp open_sqlite(path) do
    path = Path.expand(path)

    cond do
      not File.regular?(path) ->
        {:error, "no such file: #{path}"}

      true ->
        case Sqlite3.open(path, mode: :readonly) do
          {:ok, db} -> {:ok, {:sqlite, db}}
          {:error, reason} -> {:error, "can't open #{path}: #{inspect(reason)}"}
        end
    end
  end

  defp open_postgres(url) do
    opts =
      try do
        Ecto.Repo.Supervisor.parse_url(url)
      rescue
        _ -> nil
      end

    if opts do
      # a failed connect stops instead of retrying forever
      opts =
        Keyword.merge(opts,
          pool_size: 1,
          backoff_type: :stop,
          show_sensitive_data_on_connection_error: false
        )

      Process.flag(:trap_exit, true)

      with {:ok, pid} <- Postgrex.start_link(opts),
           {:ok, _} <- safe_query(pid, "SELECT 1") do
        {:ok, {:postgres, pid}}
      else
        {:error, %{message: msg}} when is_binary(msg) ->
          {:error, "can't connect to #{describe(url)}: #{msg}"}

        {:error, reason} ->
          {:error, "can't connect to #{describe(url)}: #{reason_text(reason)}"}
      end
    else
      {:error, "#{describe(url)} isn't a valid postgres:// URL"}
    end
  end

  defp safe_query(pid, sql) do
    Postgrex.query(pid, sql, [], timeout: 15_000)
  catch
    :exit, reason -> {:error, reason}
  end

  defp reason_text(%DBConnection.ConnectionError{message: m}), do: m
  defp reason_text(%{__exception__: true} = e), do: Exception.message(e)
  defp reason_text({%{__exception__: true} = e, _}), do: Exception.message(e)
  defp reason_text(_), do: "connection failed"

  @doc "The URL with any password taken out, for messages."
  @spec describe(String.t()) :: String.t()
  def describe(url) do
    case URI.parse(url) do
      %URI{userinfo: info} = uri when is_binary(info) ->
        user = info |> String.split(":", parts: 2) |> hd()
        URI.to_string(%{uri | userinfo: user})

      _ ->
        url
    end
  rescue
    _ -> "the database URL"
  end

  @spec close(t()) :: :ok
  def close({:sqlite, db}), do: Sqlite3.close(db)

  def close({:postgres, pid}) do
    GenServer.stop(pid)
    :ok
  catch
    :exit, _ -> :ok
  end

  @spec tables(t()) :: MapSet.t(String.t())
  def tables({:sqlite, _} = s),
    do: s |> all("SELECT name FROM sqlite_master WHERE type = 'table'") |> names()

  def tables({:postgres, _} = s),
    do:
      s
      |> all(
        "SELECT table_name AS name FROM information_schema.tables WHERE table_schema = current_schema()"
      )
      |> names()

  defp names(rows), do: MapSet.new(rows, & &1["name"])

  @doc "Every row of a query, as maps with string keys."
  @spec all(t(), String.t()) :: [map()]
  def all({:sqlite, db}, sql) do
    {:ok, stmt} = Sqlite3.prepare(db, sql)

    try do
      {:ok, cols} = Sqlite3.columns(db, stmt)
      {:ok, rows} = Sqlite3.fetch_all(db, stmt)
      Enum.map(rows, &(cols |> Enum.zip(&1) |> Map.new()))
    after
      Sqlite3.release(db, stmt)
    end
  end

  def all({:postgres, pid}, sql) do
    %Postgrex.Result{columns: cols, rows: rows} = Postgrex.query!(pid, sql, [])
    Enum.map(rows, &(cols |> Enum.zip(&1) |> Map.new()))
  end
end
