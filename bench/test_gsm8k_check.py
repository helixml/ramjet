import unittest

from bench import gsm8k_check


class AnswerExtractionTest(unittest.TestCase):
    def test_gold_answer_reads_the_final_marker(self):
        self.assertEqual(gsm8k_check.gold_answer("48/2 = <<48/2=24>>24\n#### 1,072"), "1072")

    def test_prefers_the_last_stated_answer(self):
        text = "The answer is 3. Wait, recheck.\nThe answer is $1,250.00"
        self.assertEqual(gsm8k_check.extract_answer(text), "1250")

    def test_falls_back_to_the_last_number(self):
        self.assertEqual(gsm8k_check.extract_answer("48 + 24 = 72 clips"), "72")

    def test_no_number_is_none(self):
        self.assertIsNone(gsm8k_check.extract_answer("I cannot solve this."))

    def test_normalizes_decimals_and_signs(self):
        self.assertEqual(gsm8k_check.normalize("-3.50"), "-3.5")
        self.assertEqual(gsm8k_check.normalize("7."), "7")


if __name__ == "__main__":
    unittest.main()
