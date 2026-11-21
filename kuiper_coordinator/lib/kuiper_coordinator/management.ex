defmodule KuiperCoordinator.Management do
  @moduledoc """
  ManagementService (`management.proto`, parity M1-M6) on a Unix socket, for the CLI's `token`
  and `agent` commands. Like the Rust coordinator it has no auth beyond the socket's file
  permissions, so the socket is owner-only (0600) in a directory we create 0700.

  It runs beside the Rust coordinator, whose socket is `<data_dir>/coordinator.sock`, so Phoenix
  defaults to `<data_dir>/phoenix-coordinator.sock` and never removes a socket something still
  answers on (the Rust one unlinks whatever is there).

  Settings: `config :kuiper_coordinator, :management, enabled: bool, socket: path`
  (KUIPER_MANAGEMENT, KUIPER_MANAGEMENT_SOCKET in `config/runtime.exs`).
  """

  @socket_name "phoenix-coordinator.sock"

  def settings, do: Application.get_env(:kuiper_coordinator, :management, [])

  @doc "Default socket path in `data_dir`, not the Rust coordinator's `coordinator.sock`."
  def default_socket(data_dir), do: Path.join(data_dir, @socket_name)

  @doc "Child spec for the application: the management socket when enabled, nothing otherwise."
  def children do
    opts = settings()
    if Keyword.get(opts, :enabled, false), do: [{__MODULE__.Supervisor, opts}], else: []
  end
end
