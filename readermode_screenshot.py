#!/usr/bin/env python3

from contextlib import closing
from os import chdir
from sys import argv
from tempfile import TemporaryDirectory
from time import sleep
from urllib.parse import quote

from markdown_strings import blockquote
from selenium.webdriver import Firefox
from selenium.webdriver.firefox.firefox_binary import FirefoxBinary
from selenium.webdriver.firefox.options import Options


def main(src, dst_img, dst_txt, firefox_binary='/usr/bin/firefox', executable_path='/usr/bin/geckodriver'):
    with TemporaryDirectory() as tmpdir:
        options = Options()
        options.add_argument('--headless')
        options.add_argument('--no-remote')
        options.add_argument('--no-profile=' + tmpdir)
        options.accept_insecure_certs = False

        driver = Firefox(
            firefox_binary=FirefoxBinary(firefox_binary),
            executable_path=executable_path,
            firefox_profile=tmpdir,
            timeout=5,
            capabilities=options.to_capabilities(),
            options=options,
            service_log_path=tmpdir + '/geckodriver.log',
        )
        with closing(driver):
            # driver.get('about:reader?url=' + quote('http://webcache.googleusercontent.com/search?q=cache:' + quote(src)))
            driver.get('about:reader?url=' + quote(src))

            sleep(5)
            driver.implicitly_wait(5)

            container = driver.find_element_by_css_selector('.container')
            driver.execute_script('''
                ;(function (v) {
                    if (v) {
                        v.style.setProperty("--font-size", "18px");
                        v.style.setProperty("--content-width", "640px");
                        v.style.setProperty("--main-foreground", "#000");
                        v.style.setProperty("--link-foreground", "#000");
                    }
                }(document.body));
            ''')
            driver.execute_script(r'''
                ;(function (v) {
                    if (v) {
                        v.style.setProperty("--line-height", "1.4em");
                    }
                }(document.querySelector('.container')));
            ''')
            driver.execute_script(r'''
                ;(function (v) {
                    if (v) {
                        v.style.display = 'none'
                    }
                }(document.querySelector('[data-component=FeatureBar]')));
            ''')
            sleep(1)

            with open(dst_txt, 'wt') as f:
                f.write(blockquote(container.text.rstrip().replace('\n', '\n\n')).rstrip())

            container.screenshot(dst_img)


if __name__ == '__main__':
    main(*argv[1:])
