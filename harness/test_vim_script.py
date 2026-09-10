import unittest

from vim_script import inspection_script, parse_script, score_registers

SCRIPT = b'''call setreg('a', "0w")
call setreg('b', "gUiw")
call setreg('c', "$a;OK\\<Esc>")
:%normal! @a
:%normal! @b
:%normal! @c
:wq
'''


class VimScriptTests(unittest.TestCase):
    def test_declared_commands_and_required_registers(self):
        definitions = parse_script(SCRIPT)
        self.assertEqual(len(definitions), 3)
        inspection = inspection_script(definitions)
        self.assertNotIn(b':%normal!', inspection)
        for missing in (b'call setreg(\'a\', "0w")\n', b':%normal! @c\n', b':wq\n'):
            self.assertIsNone(parse_script(SCRIPT.replace(missing, b'')))

    def test_ex_command_injection_and_forbidden_content(self):
        for payload in (b'call setreg(\'a\', "0w") | call setreg(\'a\', "x")',
                        b'call setreg(\'a\', ":!id")',
                        b'call setreg(\'a\', ":read /hidden")',
                        b'call setreg(\'a\', system("id"))',
                        b'call setreg(\'a\', "x")\n:source /hidden'):
            self.assertIsNone(parse_script(SCRIPT.replace(b'call setreg(\'a\', "0w")', payload)))

    def test_register_efficiency_and_distinctness(self):
        self.assertEqual(score_registers({'registers':['a','b','c'],'counts':[1,2,196]}), 1)
        for registers, counts in [(['a','b','c'],[1,2,197]),
                                  (['a','b','c'],[0,2,3]),
                                  (['a',' a ','c'],[1,2,3])]:
            self.assertEqual(score_registers({'registers':registers,'counts':counts}), 0)
        with self.assertRaises(ValueError):
            score_registers({'registers':['a','b','c'],'counts':[True,2,3]})


if __name__ == '__main__':
    unittest.main()
