type Props = { ok: boolean };

export function View({ ok }: Props) {
    return ok ? <span>{true}</span> : <span>{false}</span>;
}
