defmodule KuiperCoordinator.AgentGrpc.ProxyTransport do
  @moduledoc """
  Ranch transport for the agent gRPC port with an optional PROXY protocol v1/v2 header before
  the TLS handshake, like the Rust coordinator's `grpc.proxy_protocol` (parity A3, D74).

  Behind a load balancer that passes TLS through (Traefik TLSRoute, HAProxy, an NLB) each
  connection starts with a PROXY header naming the real client. The header is optional, as in
  Rust's `parse_proxy_protocol`: the first byte is peeked, and only a connection starting with
  `P` (v1) or `0x0D` (v2) has a header read off it; anything else (a TLS ClientHello starts with
  `0x16`) is a direct connection, so in-cluster agents can skip the balancer. The header is read
  exactly (v1 up to its CRLF, at most 107 bytes; v2 its 16 fixed bytes plus the length they
  give), never into the TLS bytes behind it, within `:proxy_header_timeout` (5 s, as Rust).
  A malformed or slow header closes the connection.

  Everything else is `:ranch_ssl`. Peeking needs the socket-backed `gen_tcp`
  (`inet_backend: socket`), which `listen/1` asks for.
  """
  @behaviour :ranch_transport
  require Logger

  @default_timeout 5_000
  @v2_sig <<"\r\n\r\n", 0, "\r\nQUIT\n">>

  @doc "Puts this transport in place of `:ranch_ssl` in a gRPC Cowboy child spec."
  def wrap_child_spec(%{start: {m, f, [scheme, endpoint, servers, port, {rm, rf, ra}]}} = spec) do
    if :ranch_ssl not in ra, do: raise(ArgumentError, "agent gRPC child spec has no :ranch_ssl")
    ra = Enum.map(ra, fn t -> if t == :ranch_ssl, do: __MODULE__, else: t end)
    %{spec | start: {m, f, [scheme, endpoint, servers, port, {rm, rf, ra}]}}
  end

  # :ranch_ssl.listen/1 with the socket-backed gen_tcp under it. ranch_ssl puts its defaults
  # (reuseaddr, ...) ahead of the given options, and inet_backend only works first, so the
  # options are put together here the same way with inet_backend in front.
  @impl true
  def listen(trans_opts) do
    :ok = cleanup(trans_opts)
    logger = Map.get(trans_opts, :logger, :logger)

    socket_opts =
      trans_opts
      |> Map.get(:socket_opts, [])
      |> :ranch.set_option_default(:backlog, 1024)
      |> :ranch.set_option_default(:nodelay, true)
      |> :ranch.set_option_default(:send_timeout, 30_000)
      |> :ranch.set_option_default(:send_timeout_close, true)
      |> :ranch.filter_options(
        [:inet_backend | :ranch_ssl.disallowed_listen_options()],
        [:binary, active: false, reuseaddr: true],
        logger
      )

    :ssl.listen(0, [{:inet_backend, :socket} | socket_opts])
  end

  @impl true
  def handshake(socket, timeout), do: handshake(socket, [], timeout)

  @impl true
  def handshake(socket, opts, timeout) do
    header_timeout =
      Application.get_env(:kuiper_coordinator, :agent_grpc, [])
      |> Keyword.get(:proxy_header_timeout, @default_timeout)

    case read_proxy_header(tcp_socket(socket), header_timeout) do
      :ok -> :ranch_ssl.handshake(socket, opts, timeout)
      {:error, _} = error -> error
    end
  end

  @doc false
  def read_proxy_header(tcp, timeout) do
    deadline = System.monotonic_time(:millisecond) + timeout

    case peek(tcp, timeout) do
      {:ok, <<first>>} when first in [?P, 0x0D] ->
        with {:ok, header} <- read_header(tcp, first, deadline),
             {:ok, info, <<>>} <- :ranch_proxy_header.parse(header) do
          Logger.debug(fn -> "agent gRPC PROXY v#{info.version} header from #{source(info)}" end)
          :ok
        else
          {:ok, _info, _rest} -> proxy_error(:trailing_bytes)
          {:error, reason} -> proxy_error(reason)
        end

      {:ok, _tls} ->
        :ok

      {:error, reason} ->
        {:error, reason}
    end
  end

  defp proxy_error(reason) do
    Logger.warning("agent gRPC: dropped a connection with a bad PROXY header: #{inspect(reason)}")
    {:error, {:proxy_header, reason}}
  end

  defp read_header(tcp, ?P, deadline), do: read_v1(tcp, <<>>, deadline)

  defp read_header(tcp, 0x0D, deadline) do
    with {:ok, <<@v2_sig, _ver_cmd, _fam, len::16>> = fixed} <- recv_exact(tcp, 16, deadline),
         {:ok, rest} <- recv_exact(tcp, len, deadline) do
      {:ok, fixed <> rest}
    else
      {:ok, _} -> {:error, :bad_v2_signature}
      error -> error
    end
  end

  defp read_v1(_tcp, acc, _deadline) when byte_size(acc) > 107, do: {:error, :v1_too_long}

  defp read_v1(tcp, acc, deadline) do
    case recv_exact(tcp, 1, deadline) do
      {:ok, "\n"} when binary_part(acc, byte_size(acc) - 1, 1) == "\r" -> {:ok, acc <> "\n"}
      {:ok, byte} -> read_v1(tcp, acc <> byte, deadline)
      error -> error
    end
  end

  defp recv_exact(_tcp, 0, _deadline), do: {:ok, <<>>}

  defp recv_exact(tcp, n, deadline) do
    case deadline - System.monotonic_time(:millisecond) do
      left when left > 0 -> :gen_tcp.recv(tcp, n, left)
      _ -> {:error, :timeout}
    end
  end

  # gen_tcp has no peek; the socket backend's :socket does (MSG_PEEK)
  defp peek({:"$inet", :gen_tcp_socket, {_pid, raw}}, timeout),
    do: :socket.recv(raw, 1, [:peek], timeout)

  defp peek(other, _timeout),
    do:
      raise(ArgumentError, "PROXY protocol needs a socket-backed gen_tcp, got #{inspect(other)}")

  # the TCP socket under an sslsocket before the handshake, as :ranch_ssl.recv_proxy_header/2
  defp tcp_socket(ssl_socket) do
    case elem(ssl_socket, 1) do
      {:gen_tcp, tcp, _, _} -> tcp
      tcp -> tcp
    end
  end

  defp source(%{src_address: addr, src_port: port}) when is_tuple(addr),
    do: "#{:inet.ntoa(addr)}:#{port}"

  defp source(_), do: "an unspecified address"

  @impl true
  defdelegate name(), to: :ranch_ssl
  @impl true
  defdelegate secure(), to: :ranch_ssl
  @impl true
  defdelegate messages(), to: :ranch_ssl
  @impl true
  defdelegate accept(lsocket, timeout), to: :ranch_ssl
  @impl true
  defdelegate handshake_continue(socket, timeout), to: :ranch_ssl
  @impl true
  defdelegate handshake_continue(socket, opts, timeout), to: :ranch_ssl
  @impl true
  defdelegate handshake_cancel(socket), to: :ranch_ssl
  @impl true
  defdelegate connect(host, port, opts), to: :ranch_ssl
  @impl true
  defdelegate connect(host, port, opts, timeout), to: :ranch_ssl
  @impl true
  defdelegate recv(socket, length, timeout), to: :ranch_ssl
  @impl true
  defdelegate recv_proxy_header(socket, timeout), to: :ranch_ssl
  @impl true
  defdelegate send(socket, packet), to: :ranch_ssl
  @impl true
  defdelegate sendfile(socket, file), to: :ranch_ssl
  @impl true
  defdelegate sendfile(socket, file, offset, bytes), to: :ranch_ssl
  @impl true
  defdelegate sendfile(socket, file, offset, bytes, opts), to: :ranch_ssl
  @impl true
  defdelegate setopts(socket, opts), to: :ranch_ssl
  @impl true
  defdelegate getopts(socket, opts), to: :ranch_ssl
  @impl true
  defdelegate getstat(socket), to: :ranch_ssl
  @impl true
  defdelegate getstat(socket, opts), to: :ranch_ssl
  @impl true
  defdelegate controlling_process(socket, pid), to: :ranch_ssl
  @impl true
  defdelegate peername(socket), to: :ranch_ssl
  @impl true
  defdelegate sockname(socket), to: :ranch_ssl
  @impl true
  defdelegate shutdown(socket, how), to: :ranch_ssl
  # ranch closes a socket whose handshake failed; one dropped for its PROXY header never
  # started TLS, and ssl can't close a transport_accept socket, so its TCP socket is closed
  @impl true
  def close(socket) do
    case :ranch_ssl.close(socket) do
      {:error, :notsup_on_transport_accept_socket} -> :gen_tcp.close(tcp_socket(socket))
      result -> result
    end
  end

  @impl true
  defdelegate cleanup(opts), to: :ranch_ssl
  @impl true
  defdelegate peercert(socket), to: :ranch_ssl
  @impl true
  defdelegate negotiated_protocol(socket), to: :ranch_ssl
  defdelegate disallowed_listen_options(), to: :ranch_ssl
  @impl true
  defdelegate format_error(reason), to: :ranch_ssl
end
