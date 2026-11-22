defmodule KuiperCoordinator.Management.Supervisor do
  @moduledoc """
  The management gRPC server on its Unix socket (see `KuiperCoordinator.Management`).

  Before listening it creates the socket's directory (0700) and clears a stale socket left by a
  crash, but refuses to start when something still answers on it, so it never takes the socket
  from a running coordinator. Once listening, the socket is made owner-only (0600).
  """
  use Supervisor
  require Logger

  def start_link(opts), do: Supervisor.start_link(__MODULE__, opts, name: name(opts))

  defp name(opts), do: Keyword.get(opts, :name, __MODULE__)

  @impl true
  def init(opts) do
    socket = Keyword.fetch!(opts, :socket) |> Path.expand()
    prepare_socket!(socket)

    # grpc_server's Cowboy adapter logs "Running ... using http://<ip>:<port>" at :info once
    # listening, via :inet.ntoa, which can't print a Unix socket address and crashes the
    # start (seen in the prod release, where the level is :info). We log our own line in
    # secure_socket/1, so keep that module's start log quiet.
    Logger.put_module_level(GRPC.Server.Adapters.Cowboy, :warning)

    children = [
      {GRPC.Server.Supervisor,
       endpoint: KuiperCoordinator.Management.Endpoint,
       port: 0,
       start_server: true,
       exception_log_filter: {KuiperCoordinator.AgentGrpc, :log_exception?},
       adapter_opts: [ip: {:local, socket}]},
      %{id: :secure_socket, start: {__MODULE__, :secure_socket, [socket]}, restart: :transient}
    ]

    Supervisor.init(children, strategy: :rest_for_one)
  end

  @doc false
  def prepare_socket!(socket) do
    dir = Path.dirname(socket)

    unless File.dir?(dir) do
      File.mkdir_p!(dir)
      File.chmod!(dir, 0o700)
    end

    case File.lstat(socket) do
      {:error, :enoent} ->
        :ok

      {:ok, %File.Stat{type: :other}} ->
        if live?(socket) do
          raise "management socket #{socket} is already in use by another process"
        end

        Logger.info("management: removing stale socket #{socket}")
        File.rm!(socket)

      {:ok, %File.Stat{type: type}} ->
        raise "management socket path #{socket} exists and is not a socket (#{type})"
    end
  end

  defp live?(socket) do
    case :gen_tcp.connect({:local, String.to_charlist(socket)}, 0, [], 500) do
      {:ok, s} ->
        :gen_tcp.close(s)
        true

      {:error, _} ->
        false
    end
  end

  @doc false
  # A start function that only chmods and returns :ignore, so it runs before init returns.
  def secure_socket(socket) do
    File.chmod!(socket, 0o600)
    Logger.info("management: listening on #{socket}")
    :ignore
  end
end
