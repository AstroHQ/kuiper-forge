defmodule KuiperCoordinator.RustImport.FigmentValue do
  @moduledoc """
  The Rust coordinator's `KUIPER_X__Y` environment, read the way figment 0.10 (`Env::prefixed("KUIPER_")
  .split("__")`) reads it, so an env-configured Rust install imports with the values Rust sees.

  `parse/1` is a port of figment's `value/parse.rs`: `true`/`false`, `{k = v}` tables, `[a, b]`
  arrays (items may be unquoted), `"..."` strings with escapes, `'c'` chars, floats (with a `.`),
  integers that fit a usize or isize, and anything else as a trimmed string. A value that doesn't
  parse whole comes back as the raw string, as figment does.
  """

  @int_max 18_446_744_073_709_551_615
  @int_min -9_223_372_036_854_775_808

  @doc "Environment variables as figment's prefixed, `__`-split provider would nest them."
  @spec env(%{String.t() => String.t()} | [{String.t(), String.t()}], String.t()) :: map()
  def env(env, prefix \\ "KUIPER_") do
    plen = byte_size(prefix)
    up = String.upcase(prefix)

    env
    |> Enum.flat_map(fn {k, v} ->
      key = String.trim(k)

      if byte_size(key) >= plen and String.upcase(binary_part(key, 0, plen)) == up do
        path =
          key
          |> binary_part(plen, byte_size(key) - plen)
          |> String.trim()
          |> String.replace("__", ".")
          |> String.downcase()
          |> String.split(".")

        if Enum.any?(path, &(&1 == "")), do: [], else: [{path, parse(v)}]
      else
        []
      end
    end)
    |> Enum.sort()
    |> Enum.reduce(%{}, fn {path, v}, acc -> merge(acc, nest(path, v)) end)
  end

  defp nest([k], v), do: %{k => v}
  defp nest([k | rest], v), do: %{k => nest(rest, v)}

  defp merge(a, b) do
    Map.merge(a, b, fn
      _k, %{} = x, %{} = y -> merge(x, y)
      _k, _x, y -> y
    end)
  end

  @doc "One environment value, parsed as figment's `Value::from_str` does."
  @spec parse(String.t()) :: term()
  def parse(s) when is_binary(s) do
    case value(s) do
      {:ok, v, ""} -> v
      _ -> s
    end
  end

  defp value(input) do
    input = skip_ws(input)

    result =
      case input do
        "true" <> rest -> {:ok, true, rest}
        "false" <> rest -> {:ok, false, rest}
        "{" <> _ -> dict(input)
        "[" <> _ -> array(input)
        "\"" <> _ -> string(input)
        "'" <> rest -> char(rest)
        _ -> bare(input)
      end

    case result do
      {:ok, v, rest} -> {:ok, v, skip_ws(rest)}
      err -> err
    end
  end

  defp char(<<c::utf8, "'", tail::binary>>), do: {:ok, <<c::utf8>>, tail}
  defp char(_), do: :error

  defp bare(input) do
    {raw, rest} = take_while(input, &(&1 not in [?,, ?{, ?}, ?[, ?]]))
    {:ok, scalar(String.trim(raw)), rest}
  end

  defp scalar(t) do
    float = String.contains?(t, ".") && float(t)

    cond do
      is_float(float) -> float
      Regex.match?(~r/\A\+?\d+\z/, t) and String.to_integer(t) <= @int_max -> String.to_integer(t)
      Regex.match?(~r/\A-\d+\z/, t) and String.to_integer(t) >= @int_min -> String.to_integer(t)
      true -> t
    end
  end

  # Rust's f64::from_str: optional sign, digits with a dot on either side, optional exponent.
  defp float(t) do
    case Regex.run(~r/\A([+-]?)(\d*)\.(\d*)([eE][+-]?\d+)?\z/, t) do
      [_, sign, int, frac | exp] when int != "" or frac != "" ->
        num = sign <> zero(int) <> "." <> zero(frac) <> List.first(exp, "")
        {f, ""} = Float.parse(num)
        f

      _ ->
        nil
    end
  end

  defp zero(""), do: "0"
  defp zero(d), do: d

  defp string("\"" <> rest) do
    case take_string(rest, []) do
      {:ok, inner, tail} ->
        case unescape(inner, []) do
          {:ok, s} -> {:ok, s, tail}
          :error -> :error
        end

      :error ->
        :error
    end
  end

  defp take_string(<<"\\", c::utf8, rest::binary>>, acc),
    do: take_string(rest, [<<c::utf8>>, "\\" | acc])

  defp take_string("\\", _acc), do: :error

  defp take_string("\"" <> rest, acc),
    do: {:ok, acc |> Enum.reverse() |> IO.iodata_to_binary(), rest}

  defp take_string(<<c::utf8, rest::binary>>, acc), do: take_string(rest, [<<c::utf8>> | acc])
  defp take_string(_, _acc), do: :error

  @escapes %{?" => "\"", ?\\ => "\\", ?b => "\b", ?f => "\f", ?n => "\n", ?r => "\r", ?t => "\t"}

  defp unescape("", acc), do: {:ok, acc |> Enum.reverse() |> IO.iodata_to_binary()}

  defp unescape(<<"\\", c, rest::binary>>, acc) when c in [?u, ?U] do
    len = if c == ?u, do: 4, else: 8

    with <<hex::binary-size(len), tail::binary>> <- rest,
         true <- Regex.match?(~r/\A[0-9a-fA-F]+\z/, hex),
         cp = String.to_integer(hex, 16),
         true <- cp <= 0x10FFFF and cp not in 0xD800..0xDFFF do
      unescape(tail, [<<cp::utf8>> | acc])
    else
      _ -> :error
    end
  end

  defp unescape(<<"\\", c, rest::binary>>, acc) do
    case @escapes do
      %{^c => s} -> unescape(rest, [s | acc])
      _ -> :error
    end
  end

  defp unescape(<<c::utf8, rest::binary>>, acc)
       when c == 0x09 or (c >= 0x20 and c != 0x7F),
       do: unescape(rest, [<<c::utf8>> | acc])

  defp unescape(_, _acc), do: :error

  defp array("[" <> rest), do: collect(rest, ?], &value/1, [])

  defp dict("{" <> rest) do
    with {:ok, pairs, tail} <- collect(rest, ?}, &key_value/1, []),
         do: {:ok, Map.new(pairs), tail}
  end

  # START (item SEPARATOR)* END, a trailing separator allowed (pear's delimited_collect)
  defp collect(<<close, rest::binary>>, close, _item, acc), do: {:ok, Enum.reverse(acc), rest}

  defp collect(input, close, item, acc) do
    case item.(input) do
      {:ok, v, "," <> rest} -> collect(rest, close, item, [v | acc])
      {:ok, v, <<^close, rest::binary>>} -> {:ok, Enum.reverse([v | acc]), rest}
      _ -> :error
    end
  end

  defp key_value(input) do
    input = skip_ws(input)

    key =
      case input do
        "\"" <> _ ->
          string(input)

        _ ->
          case take_while(
                 input,
                 &(&1 in ?a..?z or &1 in ?A..?Z or &1 in ?0..?9 or &1 in [?_, ?-])
               ) do
            {"", _} -> :error
            {k, rest} -> {:ok, k, rest}
          end
      end

    with {:ok, k, rest} <- key,
         "=" <> rest <- skip_ws(rest),
         {:ok, v, rest} <- value(rest) do
      {:ok, {k, v}, rest}
    else
      _ -> :error
    end
  end

  defp skip_ws(input), do: elem(take_while(input, &(&1 in [?\s, ?\t, ?\n, ?\r, ?\f])), 1)

  defp take_while(input, fun), do: take_while(input, fun, 0)

  defp take_while(input, fun, n) do
    case input do
      <<_::binary-size(n), c::utf8, _::binary>> ->
        if fun.(c), do: take_while(input, fun, n + byte_size(<<c::utf8>>)), else: split(input, n)

      _ ->
        split(input, n)
    end
  end

  defp split(input, n),
    do: {binary_part(input, 0, n), binary_part(input, n, byte_size(input) - n)}
end
