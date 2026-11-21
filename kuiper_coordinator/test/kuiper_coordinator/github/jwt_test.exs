defmodule KuiperCoordinator.GitHub.JWTTest do
  use ExUnit.Case, async: true

  alias KuiperCoordinator.GitHub.JWT

  setup_all do
    key = :public_key.generate_key({:rsa, 2048, 65_537})
    pem = :public_key.pem_encode([:public_key.pem_entry_encode(:RSAPrivateKey, key)])
    {:ok, key: key, pem: pem}
  end

  test "signs an RS256 app JWT: iat now-60s, exp now+9min, iss app id", %{key: key, pem: pem} do
    now = 1_700_000_000
    {:ok, jwt} = JWT.app_jwt(12345, pem, now)
    [h, c, s] = String.split(jwt, ".")

    assert %{"alg" => "RS256", "typ" => "JWT"} =
             h |> Base.url_decode64!(padding: false) |> Jason.decode!()

    assert %{"iat" => 1_699_999_940, "exp" => 1_700_000_540, "iss" => "12345"} =
             c |> Base.url_decode64!(padding: false) |> Jason.decode!()

    {:RSAPrivateKey, _, n, e, _, _, _, _, _, _, _} = key
    pub = {:RSAPublicKey, n, e}
    assert :public_key.verify("#{h}.#{c}", :sha256, Base.url_decode64!(s, padding: false), pub)
  end

  test "accepts a PEM with escaped newlines, as from an env var (#4, 8f8be0e)", %{pem: pem} do
    escaped = String.replace(pem, "\n", "\\n")
    assert {:ok, _} = JWT.app_jwt(1, escaped, 1_700_000_000)
  end

  test "a bad key is an error, not a crash" do
    assert {:error, :invalid_private_key} = JWT.app_jwt(1, "not a key", 1_700_000_000)
  end
end
