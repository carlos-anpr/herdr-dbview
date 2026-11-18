#!/usr/bin/env python3
"""Puente Oracle de solo lectura para herdr-dbview.

Uso:  python3 oracle_bridge.py <check|schema|run>   (JSON por stdin)

Escribe SIEMPRE una única línea JSON por stdout:
  {"ok": true,  "version": "...", "schema": "..."}                  (check)
  {"ok": true,  "tables": [{...}]}                                  (schema)
  {"ok": true,  "columns": [...], "rows": [...], "truncated": bool} (run)
  {"ok": false, "error": {"message", "code", "position", "connection"}}

La clave llega por stdin y nunca se escribe en argv, en el entorno ni en los mensajes.
"""

from __future__ import annotations

import datetime
import decimal
import json
import re
import sys

EXCLUDED_OWNERS = (
    "SYS",
    "SYSTEM",
    "OUTLN",
    "DIP",
    "ORACLE_OCM",
    "DBSNMP",
    "APPQOSSYS",
    "WMSYS",
    "EXFSYS",
    "CTXSYS",
    "XDB",
    "ANONYMOUS",
    "ORDSYS",
    "ORDDATA",
    "ORDPLUGINS",
    "SI_INFORMTN_SCHEMA",
    "MDSYS",
    "OLAPSYS",
    "MDDATA",
    "SPATIAL_WFS_ADMIN_USR",
    "SPATIAL_CSW_ADMIN_USR",
    "SYSMAN",
    "MGMT_VIEW",
    "APEX_030200",
    "APEX_PUBLIC_USER",
    "FLOWS_FILES",
    "OWBSYS",
    "OWBSYS_AUDIT",
)

_EXCLUDED_SQL = ", ".join("'%s'" % owner for owner in EXCLUDED_OWNERS)

# Dueña defensa de solo lectura: en Rust ya pasó `sqlguard::check`.
_FORBIDDEN = (
    "insert",
    "update",
    "delete",
    "merge",
    "upsert",
    "drop",
    "truncate",
    "alter",
    "create",
    "grant",
    "revoke",
    "commit",
    "rollback",
    "savepoint",
    "lock",
    "call",
    "execute",
    "begin",
    "declare",
    "function",
    "procedure",
    "analyze",
    "comment",
    "purge",
    "flashback",
    "rename",
)
_FORBIDDEN_RE = re.compile(r"\b(?:%s)\b" % "|".join(_FORBIDDEN), re.IGNORECASE)

_COMMENT_BLOCK = re.compile(r"/\*.*?\*/", re.DOTALL)
_COMMENT_LINE = re.compile(r"--[^\n]*")
_Q_LITERAL = re.compile(r"[nN]?[qQ]'(.)(.*?)\1'", re.DOTALL)
_STRING = re.compile(r"'(?:[^']|'')*'", re.DOTALL)
_QUOTED_IDENT = re.compile(r'"(?:[^"]|"")*"', re.DOTALL)

_CONNECTION_CODES = {
    1005,
    1017,
    1034,
    3113,
    3114,
    3135,
    12154,
    12170,
    12505,
    12514,
    12533,
    12537,
    12541,
    12542,
    12545,
    12560,
    12571,
}
_CONNECTION_MESSAGES = {
    1017: "Usuario o contraseña incorrectos en Oracle",
    1034: "La base de datos no está abierta",
    12170: "Tiempo de espera agotado al conectar con Oracle",
    12505: "El SID «{database}» no existe en el servidor",
    12514: "El servicio «{database}» no existe en el servidor",
    12541: "No se pudo conectar: no hay un listener escuchando en {host}:{port}",
    12545: "El servidor {host} no responde",
    28000: "La cuenta de Oracle está bloqueada",
}

_TYPE_NAMES = {
    "DB_TYPE_NUMBER": "NUMBER",
    "DB_TYPE_VARCHAR": "VARCHAR2",
    "DB_TYPE_NVARCHAR": "NVARCHAR2",
    "DB_TYPE_CHAR": "CHAR",
    "DB_TYPE_NCHAR": "NCHAR",
    "DB_TYPE_DATE": "DATE",
    "DB_TYPE_TIMESTAMP": "TIMESTAMP",
    "DB_TYPE_TIMESTAMP_TZ": "TIMESTAMP WITH TIME ZONE",
    "DB_TYPE_TIMESTAMP_LTZ": "TIMESTAMP WITH LOCAL TIME ZONE",
    "DB_TYPE_CLOB": "CLOB",
    "DB_TYPE_NCLOB": "NCLOB",
    "DB_TYPE_BLOB": "BLOB",
    "DB_TYPE_RAW": "RAW",
    "DB_TYPE_LONG": "LONG",
    "DB_TYPE_ROWID": "ROWID",
}

_CELL_MAX = 4096


class BridgeError(Exception):
    def __init__(self, message: str, connection: bool = False, code: str | None = None, position: int | None = None):
        super().__init__(message)
        self.message = message
        self.connection = connection
        self.code = code
        self.position = position


def emit(payload: dict) -> None:
    sys.stdout.write(json.dumps(payload, ensure_ascii=True, separators=(",", ":")) + "\n")
    sys.stdout.flush()


def emit_error(error: BridgeError) -> None:
    emit(
        {
            "ok": False,
            "error": {
                "message": error.message,
                "code": error.code,
                "position": error.position,
                "connection": error.connection,
            },
        }
    )


def clamp(value, low: int, high: int, default: int) -> int:
    try:
        number = int(value)
    except (TypeError, ValueError):
        return default
    return max(low, min(high, number))


def mask_literals(sql: str) -> str:
    sql = _COMMENT_BLOCK.sub(" ", sql)
    sql = _COMMENT_LINE.sub(" ", sql)
    sql = _Q_LITERAL.sub("''", sql)
    sql = _STRING.sub("''", sql)
    return _QUOTED_IDENT.sub('""', sql)


def validate_sql(sql: str) -> None:
    if not sql.strip():
        raise BridgeError("Escribe una consulta")
    masked = mask_literals(sql)
    statements = [part for part in masked.split(";") if part.strip()]
    if len(statements) > 1:
        raise BridgeError("Solo se permite una sentencia de solo lectura")
    body = statements[0].lstrip(" \t\r\n(")
    if not re.match(r"(?is)^(select|with)\b", body):
        raise BridgeError("Solo se permiten consultas SELECT o WITH")
    if _FORBIDDEN_RE.search(body):
        raise BridgeError("La consulta incluye una operación no permitida: esto es solo lectura")


def format_catalog_type(data_type, char_length, precision, scale) -> str:
    name = (data_type or "").upper().strip()
    if name == "NUMBER" and precision is not None:
        if scale:
            return "NUMBER(%s,%s)" % (precision, scale)
        return "NUMBER(%s)" % precision
    if name in ("VARCHAR2", "CHAR", "NVARCHAR2", "NCHAR", "RAW") and char_length:
        return "%s(%s)" % (name, char_length)
    return name


def clip(text: str) -> str:
    if len(text) > _CELL_MAX:
        return text[:_CELL_MAX] + "…"
    return text


def to_text(value):
    if value is None:
        return None
    if isinstance(value, datetime.datetime):
        return value.strftime("%Y-%m-%d %H:%M:%S")
    if isinstance(value, datetime.date):
        return value.strftime("%Y-%m-%d")
    if isinstance(value, datetime.timedelta):
        return str(value)
    if isinstance(value, decimal.Decimal):
        return str(value)
    if isinstance(value, bool):
        return "true" if value else "false"
    if isinstance(value, bytes):
        return clip("0x" + value.hex())
    if hasattr(value, "read"):
        try:
            data = value.read(1, _CELL_MAX)
        except Exception:
            data = value.read()
        if isinstance(data, bytes):
            return clip("0x" + data.hex())
        return clip(str(data))
    return clip(str(value))


def translate(exc: Exception, phase: str, payload: dict) -> BridgeError:
    if isinstance(exc, BridgeError):
        return exc
    if isinstance(exc, ImportError):
        return BridgeError("Falta el módulo oracledb en el intérprete de Python configurado para Oracle", True)
    info = exc.args[0] if getattr(exc, "args", None) else None
    code = getattr(info, "code", None)
    message = getattr(info, "message", None) or str(exc)
    text = str(message).split("\n")[0].strip() or "Error de Oracle"
    ora = "ORA-%05d" % code if isinstance(code, int) else None
    if phase == "connect":
        friendly = _CONNECTION_MESSAGES.get(code) if isinstance(code, int) else None
        if friendly:
            message = friendly.format(host=payload.get("host", "?"), port=payload.get("port", "?"), database=payload.get("database", "?"))
            if ora:
                message = "%s (%s)" % (message, ora)
        else:
            message = "No se pudo conectar con Oracle: %s" % text
        return BridgeError(message, True, ora)
    if isinstance(code, int) and code in _CONNECTION_CODES:
        return BridgeError("Se perdió la conexión con Oracle: %s" % text, True, ora)
    if "timeout" in text.lower() or "tiempo de espera" in text.lower() or "DPY-4024" in text or "DPI-1067" in text:
        return BridgeError("La consulta superó el tiempo máximo configurado", False, ora)
    offset = getattr(info, "offset", None)
    position = offset + 1 if isinstance(offset, int) and offset > 0 else None
    return BridgeError(text, False, ora, position)


def connect(payload: dict):
    try:
        import oracledb
    except ImportError as exc:  # pragma: no cover - depende del intérprete configurado
        raise BridgeError("Falta el módulo oracledb en el intérprete de Python configurado para Oracle", True) from exc

    host = str(payload.get("host") or "")
    port = clamp(payload.get("port"), 1, 65535, 1521)
    database = str(payload.get("database") or "")
    user = str(payload.get("user") or "")
    password = str(payload.get("password") or "")
    timeout = clamp(payload.get("timeout_secs"), 1, 3600, 30)
    if not host or not database or not user:
        raise BridgeError("Faltan datos de conexión de Oracle (host, base de datos o usuario)", True)

    oracledb.defaults.fetch_lobs = False
    try:
        if payload.get("service"):
            dsn = oracledb.makedsn(host, port, service_name=database)
        else:
            dsn = oracledb.makedsn(host, port, sid=database)
        connection = oracledb.connect(user=user, password=password, dsn=dsn, tcp_connect_timeout=min(timeout, 30))
        connection.call_timeout = timeout * 1000
        return connection
    except Exception as exc:
        raise translate(exc, "connect", payload) from exc


def op_check(payload: dict) -> dict:
    connection = connect(payload)
    try:
        cursor = connection.cursor()
        version = None
        try:
            cursor.execute("SELECT banner FROM v$version WHERE ROWNUM = 1")
            row = cursor.fetchone()
            if row and row[0]:
                version = str(row[0]).strip()
        except Exception:
            version = None
        if not version:
            version = "Oracle Database %s" % connection.version
        schema = None
        try:
            cursor.execute("SELECT SYS_CONTEXT('USERENV','CURRENT_SCHEMA') FROM dual")
            row = cursor.fetchone()
            if row and row[0]:
                schema = str(row[0]).strip()
        except Exception:
            schema = None
        return {"version": version, "schema": schema}
    finally:
        connection.close()


def op_schema(payload: dict) -> dict:
    connection = connect(payload)
    try:
        cursor = connection.cursor()
        cursor.arraysize = 500
        tables = {}

        cursor.execute(
            "SELECT owner, table_name, 'TABLE', num_rows FROM all_tables WHERE owner NOT IN (%s) "
            "UNION ALL "
            "SELECT owner, view_name, 'VIEW', NULL FROM all_views WHERE owner NOT IN (%s)"
            % (_EXCLUDED_SQL, _EXCLUDED_SQL)
        )
        for owner, name, kind, num_rows in cursor:
            tables[(owner, name)] = {"schema": owner, "name": name, "kind": kind, "comment": None,
                                     "columns": [], "rows": int(num_rows) if kind == "TABLE" and num_rows is not None and num_rows >= 0 else None, "fks": []}

        cursor.execute(
            "SELECT c.owner, c.table_name, c.column_name, c.data_type, c.char_length, "
            "c.data_precision, c.data_scale, c.nullable, cm.comments "
            "FROM all_tab_columns c LEFT JOIN all_col_comments cm "
            "ON cm.owner = c.owner AND cm.table_name = c.table_name AND cm.column_name = c.column_name WHERE c.owner NOT IN (%s) "
            "ORDER BY c.owner, c.table_name, c.column_id" % _EXCLUDED_SQL
        )
        for owner, table, column, data_type, char_length, precision, scale, nullable, comment in cursor:
            entry = tables.get((owner, table))
            if entry is None:
                continue  # No inventar tablas a partir de otros objetos del diccionario.
            entry["columns"].append(
                {
                    "name": column,
                    "type": format_catalog_type(data_type, char_length, precision, scale),
                    "nullable": nullable == "Y",
                    "pk": False,
                    "unique": False,
                    "comment": comment.strip() if comment and comment.strip() else None,
                }
            )

        cursor.execute(
            "SELECT owner, table_name, comments FROM all_tab_comments "
            "WHERE owner NOT IN (%s) AND comments IS NOT NULL" % _EXCLUDED_SQL
        )
        for owner, table, comment in cursor:
            entry = tables.get((owner, table))
            if entry is not None and comment and comment.strip():
                entry["comment"] = comment.strip()

        cursor.execute(
            "SELECT c.owner, c.table_name, c.constraint_name, c.constraint_type, cc.column_name "
            "FROM all_constraints c JOIN all_cons_columns cc "
            "ON c.owner = cc.owner AND c.constraint_name = cc.constraint_name AND c.table_name = cc.table_name "
            "WHERE c.constraint_type IN ('P','U') AND c.owner NOT IN (%s) "
            "ORDER BY c.owner, c.table_name, c.constraint_name, cc.position" % _EXCLUDED_SQL
        )
        keys = {}
        for owner, table, constraint, kind, column in cursor:
            keys.setdefault((owner, table, constraint, kind), []).append(column)
        primary_keys, unique_keys = set(), set()
        for (owner, table, _, kind), columns in keys.items():
            if kind == "P":
                primary_keys.update((owner, table, col) for col in columns)
            if len(columns) == 1:
                unique_keys.add((owner, table, columns[0]))
        for (owner, table), entry in tables.items():
            if entry["kind"] == "VIEW":
                continue
            for column in entry["columns"]:
                key = (owner, table, column["name"])
                column["pk"] = key in primary_keys
                column["unique"] = key in unique_keys

        cursor.execute(
            "SELECT c.owner, c.table_name, c.constraint_name, cc.column_name, "
            "r.owner, r.table_name, rc.column_name "
            "FROM all_constraints c "
            "JOIN all_cons_columns cc ON cc.owner = c.owner AND cc.constraint_name = c.constraint_name AND cc.table_name = c.table_name "
            "JOIN all_constraints r ON r.owner = c.r_owner AND r.constraint_name = c.r_constraint_name "
            "JOIN all_cons_columns rc ON rc.owner = r.owner AND rc.constraint_name = r.constraint_name "
            "AND rc.table_name = r.table_name AND rc.position = cc.position "
            "WHERE c.constraint_type = 'R' AND c.owner NOT IN (%s) "
            "ORDER BY c.owner, c.table_name, c.constraint_name, cc.position" % _EXCLUDED_SQL
        )
        foreign_keys = {}
        for owner, table, constraint, column, ref_owner, ref_table, ref_column in cursor:
            entry = tables.get((owner, table))
            if entry is None or entry["kind"] == "VIEW":
                continue
            key = (owner, table, constraint)
            if key not in foreign_keys:
                fk = {"columns": [], "ref_schema": ref_owner, "ref_table": ref_table, "ref_columns": []}
                foreign_keys[key] = fk
                entry["fks"].append(fk)
            foreign_keys[key]["columns"].append(column)
            foreign_keys[key]["ref_columns"].append(ref_column)

        return {"tables": sorted(tables.values(), key=lambda entry: (entry["schema"], entry["name"]))}
    finally:
        connection.close()


def column_meta(cursor) -> list:
    if not cursor.description:
        return []
    columns = []
    for description in cursor.description:
        name = str(description[0])
        raw = description[1]
        db_type = getattr(raw, "name", None) or str(raw)
        columns.append({"name": name, "type": _TYPE_NAMES.get(db_type, db_type.replace("DB_TYPE_", ""))})
    return columns


def op_run(payload: dict) -> dict:
    sql = str(payload.get("sql") or "").strip()
    validate_sql(sql)
    limit = clamp(payload.get("limit"), 1, 10000, 500)
    connection = connect(payload)
    try:
        cursor = connection.cursor()
        cursor.arraysize = min(limit + 1, 500)
        cursor.execute(sql)
        rows = cursor.fetchmany(limit + 1)
        truncated = len(rows) > limit
        if truncated:
            rows = rows[:limit]
        return {
            "columns": column_meta(cursor),
            "rows": [[to_text(value) for value in row] for row in rows],
            "truncated": truncated,
        }
    finally:
        connection.close()


def main(argv) -> int:
    operation = argv[1] if len(argv) > 1 else ""
    try:
        raw = sys.stdin.read()
        payload = json.loads(raw) if raw.strip() else {}
        if not isinstance(payload, dict):
            raise ValueError
    except ValueError:
        emit_error(BridgeError("La petición del puente Oracle no es válida"))
        return 2

    operations = {"check": op_check, "schema": op_schema, "run": op_run}
    handler = operations.get(operation)
    if handler is None:
        emit_error(BridgeError("Operación desconocida del puente Oracle: %r" % operation))
        return 2
    try:
        result = handler(payload)
    except Exception as exc:
        emit_error(translate(exc, "query", payload))
        return 0
    emit({"ok": True, **result})
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
